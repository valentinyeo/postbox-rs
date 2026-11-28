fn main() {
    println!("cargo:rerun-if-changed=../../assets/messages.ico");

    #[cfg(target_os = "windows")]
    winresource::WindowsResource::new()
        .set_icon_with_id("../../assets/messages.ico", "1")
        .set("ProductName", "Postbox")
        .set("FileDescription", "Postbox")
        .compile()
        .expect("compile Windows icon resource");
}
