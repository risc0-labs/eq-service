fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Running build.rs");
    let code_gen_path = std::path::Path::new("src/generated");
    if !code_gen_path.exists() {
        std::fs::create_dir_all(code_gen_path)?;
    }
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .out_dir(code_gen_path)
        .compile(&["proto/eqservice.proto"], &["proto/"])?;
    Ok(())
}
