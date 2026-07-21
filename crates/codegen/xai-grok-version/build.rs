fn main() {
    println!("cargo:rerun-if-env-changed=ATLAS_VERSION");
}
