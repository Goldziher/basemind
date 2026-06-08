fn main() {
    println!("cargo:rerun-if-changed=schema/basemind-config-v1.schema.json");
    println!("cargo:rerun-if-changed=src/queries");
}
