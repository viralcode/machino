//! `machino synth` — generates random, verified machino programs.
//!
//! New languages have zero training data, which is the main obstacle to LLM
//! adoption. This module attacks that directly: it generates random programs
//! from the grammar, and only emits ones that pass the type checker, so every
//! sample in the corpus is valid, compilable machino. Pair each sample with
//! its interpreter output to build (program, behavior) training pairs.

use std::fmt::Write;

/// Small deterministic PRNG (xorshift64*), so corpora are reproducible.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn range(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.range(items.len() as u64) as usize]
    }
}

const NAMES: &[&str] = &[
    "acc", "base", "count", "delta", "extra", "factor", "gap", "high", "item", "low", "mid",
    "num", "offset", "pace", "quota", "rate", "size", "step", "total", "unit",
];

const FN_NAMES: &[&str] = &[
    "combine", "scale", "shift", "measure", "adjust", "blend", "reduce", "amplify", "clamp_pos",
    "weigh",
];

/// Generates one random program. Every generated program is closed over a
/// small int-expression language, so it is well-typed by construction; the
/// caller still runs the real type checker as a belt-and-braces filter.
pub fn generate(rng: &mut Rng) -> String {
    let mut out = String::new();
    let n_fns = 1 + rng.range(3) as usize;
    let mut fns: Vec<(String, usize)> = Vec::new();

    for i in 0..n_fns {
        let name = format!("{}_{}", FN_NAMES[rng.range(FN_NAMES.len() as u64) as usize], i);
        let n_params = 1 + rng.range(2) as usize;
        let params: Vec<String> = (0..n_params)
            .map(|j| NAMES[(rng.range(10) as usize + j * 7) % NAMES.len()].to_string())
            .collect();
        let sig_params = params
            .iter()
            .map(|p| format!("{}: int", p))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(out, "fn {}({}) -> int {{", name, sig_params).unwrap();

        let n_lets = rng.range(3) as usize;
        let mut vars: Vec<String> = params.clone();
        for _ in 0..n_lets {
            let v = format!("v{}", rng.range(1000));
            let e = gen_int_expr(rng, &vars, &fns, 2);
            writeln!(out, "    let {} = {}", v, e).unwrap();
            vars.push(v);
        }
        if rng.range(2) == 0 {
            let cond_a = rng.pick(&vars).clone();
            let thr = rng.range(20) as i64;
            let then_e = gen_int_expr(rng, &vars, &fns, 1);
            let else_e = gen_int_expr(rng, &vars, &fns, 1);
            writeln!(out, "    if {} > {} {{", cond_a, thr).unwrap();
            writeln!(out, "        return {}", then_e).unwrap();
            writeln!(out, "    }}").unwrap();
            writeln!(out, "    return {}", else_e).unwrap();
        } else {
            let e = gen_int_expr(rng, &vars, &fns, 2);
            writeln!(out, "    return {}", e).unwrap();
        }
        writeln!(out, "}}").unwrap();
        writeln!(out).unwrap();
        fns.push((name, n_params));
    }

    // main prints a few calls so the program has observable behavior
    writeln!(out, "fn main() {{").unwrap();
    for _ in 0..(1 + rng.range(3)) {
        let (name, arity) = rng.pick(&fns).clone();
        let args: Vec<String> = (0..arity).map(|_| rng.range(30).to_string()).collect();
        writeln!(out, "    print({}({}))", name, args.join(", ")).unwrap();
    }
    writeln!(out, "}}").unwrap();
    out
}

fn gen_int_expr(
    rng: &mut Rng,
    vars: &[String],
    fns: &[(String, usize)],
    depth: u32,
) -> String {
    if depth == 0 {
        return match rng.range(2) {
            0 if !vars.is_empty() => rng.pick(vars).clone(),
            _ => rng.range(50).to_string(),
        };
    }
    match rng.range(5) {
        0 if !vars.is_empty() => rng.pick(vars).clone(),
        1 => rng.range(50).to_string(),
        2 if !fns.is_empty() => {
            let (name, arity) = rng.pick(fns).clone();
            let args: Vec<String> = (0..arity)
                .map(|_| gen_int_expr(rng, vars, fns, depth - 1))
                .collect();
            format!("{}({})", name, args.join(", "))
        }
        _ => {
            let op = *rng.pick(&["+", "-", "*"]);
            let a = gen_int_expr(rng, vars, fns, depth - 1);
            let b = gen_int_expr(rng, vars, fns, depth - 1);
            format!("({} {} {})", a, op, b)
        }
    }
}
