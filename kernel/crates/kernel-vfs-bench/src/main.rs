//! Layer N host runner — prints one JSON line per bench to stdout.
//! Gates inside each line carry `pass` flags; exit code 1 if any failed.

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    let mut fail = 0;
    let mut total = 0;
    println!("{{\"id\":\"META\",\"layer\":\"{}\",\"rustc\":\"{}\"}}", kernel_vfs_bench::layer_name(), "nightly-1.102");
    let args: Vec<String> = std::env::args().collect();
    let ids: Vec<u32> = if args.len() > 1 {
        args[1..].iter().map(|a| a.parse().unwrap()).collect()
    } else {
        kernel_vfs_bench::BENCH_IDS.to_vec()
    };
    for id in ids {
            let line = kernel_vfs_bench::run_bench(id).unwrap_or_else(|| {
            format!("{{\"id\":\"B{}\",\"error\":\"missing\"}}", id)
        });
        // Tally gates.
        let mut pass = 0;
        let mut idx = 0;
        while let Some(a) = line[idx..].find("\"pass\":") {
            let rest = &line[idx + a + 7..];
            if rest.starts_with("true") {
                pass += 1;
            } else if rest.starts_with("false") {
                fail += 1;
            }
            idx += a + 7;
        }
        total += pass;
        println!("{}", line);
    }
    println!(
        "{{\"id\":\"SUMMARY\",\"gates_pass\":{},\"gates_fail\":{},\"all_pass\":{}}}",
        total, fail, fail == 0
    );
    if fail > 0 {
        std::process::exit(1);
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {}
