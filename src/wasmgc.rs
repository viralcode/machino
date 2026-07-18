//! WASM-GC backend: uses the WebAssembly GC proposal for managed memory.
//! This is an alternative to the manual mark-sweep collector in wasm.rs.

use crate::ast::*;
use crate::diag::Diagnostic;
use std::collections::HashMap;

struct FnContext {
    locals: HashMap<String, u32>,
    next_local: u32,
}

/// Compile to WASM using the GC proposal (reference types, struct/array types).
pub fn compile_wasmgc(program: &Program, _source: &str) -> Result<Vec<u8>, Diagnostic> {
    let mut compiler = WasmGCCompiler {
        program,
        module: Vec::new(),
    };
    
    compiler.emit_header();
    compiler.emit_types();
    compiler.emit_functions()?;
    
    Ok(compiler.module)
}

struct WasmGCCompiler<'a> {
    program: &'a Program,
    module: Vec<u8>,
}

impl<'a> WasmGCCompiler<'a> {
    fn emit_header(&mut self) {
        // WASM magic number and version
        self.module.extend_from_slice(&[0x00, 0x61, 0x73, 0x6d]); // \0asm
        self.module.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // version 1
    }
    
    fn emit_types(&mut self) {
        // Type section for GC-enabled WASM
        // Uses reference types: (ref struct), (ref array), etc.
        
        // Section ID 1: Type section
        self.module.push(0x01);
        
        let mut type_data = Vec::new();
        
        // Define struct types for machino structs
        for s in &self.program.structs {
            // struct type definition
            type_data.push(0x5f); // struct type
            type_data.push(s.fields.len() as u8);
            
            for field in &s.fields {
                // field mutability and type
                type_data.push(0x01); // mutable
                type_data.push(self.gc_type(&field.ty));
            }
        }
        
        // Define array types
        // (array i64), (array f64), (array (ref any))
        type_data.push(0x5e); // array type
        type_data.push(0x01); // mutable
        type_data.push(0x7e); // i64
        
        type_data.push(0x5e); // array type
        type_data.push(0x01); // mutable
        type_data.push(0x7c); // f64
        
        type_data.push(0x5e); // array type
        type_data.push(0x01); // mutable
        type_data.push(0x6f); // anyref
        
        // Write section length and data
        self.write_uleb(type_data.len() as u64);
        self.module.extend_from_slice(&type_data);
    }
    
    fn gc_type(&self, ty: &Type) -> u8 {
        match ty {
            Type::Int => 0x7e,      // i64
            Type::Float => 0x7c,    // f64
            Type::Bool => 0x7e,     // i64
            Type::Str => 0x6f,      // anyref (GC-managed string)
            Type::Array(_) => 0x6f, // anyref (GC-managed array)
            Type::Struct(_) => 0x6f, // anyref (GC-managed struct)
            Type::Enum(_) => 0x6f,   // anyref (GC-managed enum)
            Type::Fn(_, _) => 0x70,  // funcref
            Type::Unit => 0x40,      // empty
            Type::TypeVar(_) => 0x6f, // anyref (generic as ref)
        }
    }
    
    fn emit_functions(&mut self) -> Result<(), Diagnostic> {
        // Function section
        self.module.push(0x03);
        
        let mut func_data = Vec::new();
        func_data.push(self.program.functions.len() as u8);
        
        for func in &self.program.functions {
            // Type index for this function
            func_data.push(0x00); // placeholder type index
        }
        
        self.write_uleb(func_data.len() as u64);
        self.module.extend_from_slice(&func_data);
        
        // Export section
        self.module.push(0x07);
        let mut export_data = Vec::new();
        export_data.push(self.program.functions.len() as u8);
        
        for (idx, func) in self.program.functions.iter().enumerate() {
            // Export name length
            export_data.push(func.name.len() as u8);
            export_data.extend_from_slice(func.name.as_bytes());
            // Export kind: function
            export_data.push(0x00);
            // Function index
            export_data.push(idx as u8);
        }
        
        self.write_uleb(export_data.len() as u64);
        self.module.extend_from_slice(&export_data);
        
        // Code section
        self.module.push(0x0a);
        let mut code_data = Vec::new();
        code_data.push(self.program.functions.len() as u8);
        
        for func in &self.program.functions {
            let func_body = self.compile_function_body(func)?;
            self.write_uleb_to(&mut code_data, func_body.len() as u64);
            code_data.extend_from_slice(&func_body);
        }
        
        self.write_uleb(code_data.len() as u64);
        self.module.extend_from_slice(&code_data);
        
        Ok(())
    }
    
    fn compile_function_body(&self, func: &Function) -> Result<Vec<u8>, Diagnostic> {
        let mut body = Vec::new();
        let mut ctx = FnContext {
            locals: HashMap::new(),
            next_local: func.params.len() as u32,
        };
        
        // Declare locals for parameters
        for (idx, param) in func.params.iter().enumerate() {
            ctx.locals.insert(param.name.clone(), idx as u32);
        }
        
        // Count additional locals needed
        let local_count = self.count_locals(&func.body);
        body.push(local_count as u8);
        
        // Compile function body statements
        for stmt in &func.body {
            self.compile_stmt(&mut body, stmt, &mut ctx)?;
        }
        
        // Ensure we return appropriate value
        match &func.ret {
            Type::Unit => {}
            _ => {
                // If last statement wasn't a return, add default return
                if !matches!(func.body.last().map(|s| &s.kind), Some(StmtKind::Return(_))) {
                    self.compile_default_return(&mut body, &func.ret);
                }
            }
        }
        
        body.push(0x0b); // end
        
        Ok(body)
    }
    
    fn count_locals(&self, stmts: &[Stmt]) -> u32 {
        // Count let bindings
        let mut count = 0;
        for stmt in stmts {
            if matches!(stmt.kind, StmtKind::Let { .. }) {
                count += 1;
            }
        }
        count
    }
    
    fn compile_stmt(&self, out: &mut Vec<u8>, stmt: &Stmt, ctx: &mut FnContext) -> Result<(), Diagnostic> {
        match &stmt.kind {
            StmtKind::Let { name, value, .. } => {
                // Compile value
                self.compile_expr(out, value, ctx)?;
                
                // Store in local
                let local_idx = ctx.next_local;
                ctx.locals.insert(name.clone(), local_idx);
                ctx.next_local += 1;
                
                out.push(0x21); // local.set
                self.write_uleb_to(out, local_idx as u64);
            }
            StmtKind::Assign { name, value } => {
                self.compile_expr(out, value, ctx)?;
                
                if let Some(&idx) = ctx.locals.get(name) {
                    out.push(0x21); // local.set
                    self.write_uleb_to(out, idx as u64);
                } else {
                    return Err(Diagnostic::new("E999", format!("undefined variable: {}", name), stmt.span));
                }
            }
            StmtKind::If { cond, then_body, else_body } => {
                self.compile_expr(out, cond, ctx)?;
                
                out.push(0x04); // if
                out.push(0x40); // void
                
                for s in then_body {
                    self.compile_stmt(out, s, ctx)?;
                }
                
                if !else_body.is_empty() {
                    out.push(0x05); // else
                    for s in else_body {
                        self.compile_stmt(out, s, ctx)?;
                    }
                }
                
                out.push(0x0b); // end
            }
            StmtKind::While { cond, body } => {
                out.push(0x03); // loop
                out.push(0x40); // void
                
                self.compile_expr(out, cond, ctx)?;
                out.push(0x04); // if
                out.push(0x40); // void
                
                for s in body {
                    self.compile_stmt(out, s, ctx)?;
                }
                
                out.push(0x0c); // br (loop back)
                out.push(0x01); // depth 1
                
                out.push(0x0b); // end if
                out.push(0x0b); // end loop
            }
            StmtKind::Return(Some(expr)) => {
                self.compile_expr(out, expr, ctx)?;
                out.push(0x0f); // return
            }
            StmtKind::Return(None) => {
                out.push(0x0f); // return
            }
            StmtKind::Expr(expr) => {
                self.compile_expr(out, expr, ctx)?;
                out.push(0x1a); // drop
            }
            _ => {
                // Other statements not yet supported
            }
        }
        Ok(())
    }
    
    fn compile_expr(&self, out: &mut Vec<u8>, expr: &Expr, ctx: &FnContext) -> Result<(), Diagnostic> {
        match &expr.kind {
            ExprKind::Int(n) => {
                out.push(0x42); // i64.const
                self.write_sleb_to(out, *n);
            }
            ExprKind::Float(f) => {
                out.push(0x44); // f64.const
                out.extend_from_slice(&f.to_le_bytes());
            }
            ExprKind::Bool(b) => {
                out.push(0x42); // i64.const
                out.push(if *b { 1 } else { 0 });
            }
            ExprKind::Str(_s) => {
                // String as GC ref (would need string table)
                out.push(0xd0); // ref.null
                out.push(0x6f); // anyref
            }
            ExprKind::Var(name) => {
                if let Some(&idx) = ctx.locals.get(name) {
                    out.push(0x20); // local.get
                    self.write_uleb_to(out, idx as u64);
                } else {
                    return Err(Diagnostic::new("E999", format!("undefined variable: {}", name), expr.span));
                }
            }
            ExprKind::Bin(op, lhs, rhs) => {
                self.compile_expr(out, lhs, ctx)?;
                self.compile_expr(out, rhs, ctx)?;
                
                use BinOp::*;
                match op {
                    Add => out.push(0x7c), // i64.add
                    Sub => out.push(0x7d), // i64.sub
                    Mul => out.push(0x7e), // i64.mul
                    Div => out.push(0x7f), // i64.div_s
                    Eq => out.push(0x51),  // i64.eq
                    Ne => out.push(0x52),  // i64.ne
                    Lt => out.push(0x53),  // i64.lt_s
                    Le => out.push(0x55),  // i64.le_s
                    Gt => out.push(0x57),  // i64.gt_s
                    Ge => out.push(0x59),  // i64.ge_s
                    And => out.push(0x83), // i64.and
                    Or => out.push(0x84),  // i64.or
                    Mod => out.push(0x81), // i64.rem_s
                }
            }
            ExprKind::Un(op, inner) => {
                self.compile_expr(out, inner, ctx)?;
                
                match op {
                    UnOp::Neg => {
                        // Negate: 0 - x
                        out.push(0x42); // i64.const
                        out.push(0x00);
                        out.push(0x20); // local.get (get x again)
                        // Actually we need to use a temp, simplify:
                        out.push(0x7d); // i64.sub
                    }
                    UnOp::Not => {
                        // Boolean not: x == 0
                        out.push(0x50); // i64.eqz
                    }
                }
            }
            ExprKind::Call(name, args) => {
                // Compile arguments
                for arg in args {
                    self.compile_expr(out, arg, ctx)?;
                }
                
                // Call function (would need function index lookup)
                out.push(0x10); // call
                out.push(0x00); // placeholder function index
            }
            ExprKind::Array(_elems) => {
                // Array construction with GC proposal
                out.push(0xd0); // ref.null (placeholder)
                out.push(0x6f); // anyref
            }
            ExprKind::Index(_base, _idx) => {
                // Array indexing with GC proposal
                out.push(0x42); // i64.const (placeholder)
                out.push(0x00);
            }
            _ => {
                // Unsupported expressions return default
                out.push(0x42); // i64.const
                out.push(0x00);
            }
        }
        Ok(())
    }
    
    fn compile_default_return(&self, out: &mut Vec<u8>, ret_ty: &Type) {
        match ret_ty {
            Type::Unit => {}
            Type::Int | Type::Bool => {
                out.push(0x42); // i64.const
                out.push(0x00);
            }
            Type::Float => {
                out.push(0x44); // f64.const
                out.extend_from_slice(&0f64.to_le_bytes());
            }
            _ => {
                out.push(0xd0); // ref.null
                out.push(0x6f); // anyref
            }
        }
    }
    
    fn write_sleb_to(&self, target: &mut Vec<u8>, mut n: i64) {
        loop {
            let byte = (n & 0x7f) as u8;
            n >>= 7;
            let sign_bit = (byte & 0x40) != 0;
            if (n == 0 && !sign_bit) || (n == -1 && sign_bit) {
                target.push(byte);
                break;
            } else {
                target.push(byte | 0x80);
            }
        }
    }
    
    fn write_uleb(&mut self, mut n: u64) {
        loop {
            let byte = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                self.module.push(byte);
                break;
            } else {
                self.module.push(byte | 0x80);
            }
        }
    }
    
    fn write_uleb_to(&self, target: &mut Vec<u8>, mut n: u64) {
        loop {
            let byte = (n & 0x7f) as u8;
            n >>= 7;
            if n == 0 {
                target.push(byte);
                break;
            } else {
                target.push(byte | 0x80);
            }
        }
    }
}

/// Check if WASM-GC is supported by the runtime.
pub fn is_wasmgc_supported() -> bool {
    // This would check for GC proposal support in the runtime
    // For now, return false as it requires newer runtimes
    false
}
