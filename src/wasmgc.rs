//! WASM-GC backend: uses the WebAssembly GC proposal for managed memory.
//! This is an alternative to the manual mark-sweep collector in wasm.rs.

use crate::ast::*;
use crate::diag::Diagnostic;

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
        
        // Local variables count
        body.push(0x00);
        
        // Function body (placeholder - return default value)
        match &func.ret {
            Type::Unit => {}
            Type::Int | Type::Bool => {
                body.push(0x42); // i64.const
                body.push(0x00); // 0
            }
            Type::Float => {
                body.push(0x44); // f64.const
                body.extend_from_slice(&0f64.to_le_bytes());
            }
            _ => {
                body.push(0xd0); // ref.null
                body.push(0x6f); // anyref
            }
        }
        
        body.push(0x0b); // end
        
        Ok(body)
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
