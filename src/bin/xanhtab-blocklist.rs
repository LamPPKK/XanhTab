use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use xanhtab::blocklist::compile_hosts;

#[derive(Parser)]
#[command(
    version,
    about = "Compile validated hosts files into a read-only XanhTab FST"
)]
struct Args {
    #[arg(long = "input", required = true)]
    inputs: Vec<PathBuf>,
    #[arg(long)]
    output: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let count = compile_hosts(&args.inputs, &args.output)?;
    println!(
        "compiled {count} unique hostnames into {}",
        args.output.display()
    );
    Ok(())
}
