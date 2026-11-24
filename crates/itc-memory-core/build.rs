//! Embed the operator's local allowlist when it exists.

use std::{env, fs, io, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("missing manifest path")?);
    let source = manifest.join("../../config/index-roots.txt");
    println!("cargo:rerun-if-changed={}", source.display());
    let roots = match fs::read(&source) {
        Ok(contents) if !contents.iter().all(u8::is_ascii_whitespace) => contents,
        Ok(_) => return Err("local index roots file is empty".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            println!("cargo:warning=no local index roots file; defaulting to the current checkout");
            Vec::new()
        }
        Err(error) => return Err(error.into()),
    };
    let output = PathBuf::from(env::var_os("OUT_DIR").ok_or("missing build output directory")?);
    fs::write(output.join("index-roots.txt"), roots)?;
    Ok(())
}
