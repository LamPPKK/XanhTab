use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;
use xanhtab::blocklist::{compile_hosts, merge_hosts_with_base, validate_fst_file};

#[derive(Parser)]
#[command(
    version,
    about = "Compile validated hosts files into a read-only XanhTab FST"
)]
struct Args {
    #[arg(long = "input", required_unless_present = "check_fst")]
    inputs: Vec<PathBuf>,
    #[arg(long, requires = "output", conflicts_with = "check_fst")]
    base_fst: Option<PathBuf>,
    #[arg(long, required_unless_present = "check_fst")]
    output: Option<PathBuf>,
    #[arg(long, value_name = "FILE", conflicts_with_all = ["inputs", "base_fst", "output"])]
    check_fst: Option<PathBuf>,
    #[arg(long, requires = "check_fst")]
    require_non_empty: bool,
    #[arg(long, value_name = "COUNT", requires = "check_fst")]
    expected_count: Option<usize>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Some(path) = args.check_fst {
        let count = validate_fst_file(&path)?;
        if args.require_non_empty && count == 0 {
            bail!("blocklist FST must contain at least one hostname");
        }
        if let Some(expected) = args.expected_count
            && expected != count
        {
            bail!("blocklist FST contains {count} hostnames but metadata declares {expected}");
        }
        println!("validated {count} unique hostnames in {}", path.display());
        return Ok(());
    }
    let output = args.output.expect("clap requires --output");
    let count = if let Some(base_fst) = args.base_fst {
        merge_hosts_with_base(base_fst, &args.inputs, &output)?
    } else {
        compile_hosts(&args.inputs, &output)?
    };
    println!(
        "compiled {count} unique hostnames into {}",
        output.display()
    );
    Ok(())
}
