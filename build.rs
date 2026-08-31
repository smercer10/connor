fn main() {
    let src = std::path::Path::new("grammars/dockerfile/src");
    cc::Build::new()
        .include(src)
        .file(src.join("parser.c"))
        .file(src.join("scanner.c"))
        .warnings(false) // generated C, not ours to fix
        .compile("tree-sitter-dockerfile");
    println!("cargo:rerun-if-changed=grammars/dockerfile/src");
}
