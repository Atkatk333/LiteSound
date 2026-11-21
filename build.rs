fn main() {
    println!("cargo:rerun-if-changed=assets/litesound.ico");
    println!("cargo:rerun-if-changed=windows.rc");
    // 返回值的唯一用途是配合 manifest 参数报错；我们没传 manifest，链接指令在 compile 内部已发出
    let _ = embed_resource::compile("windows.rc", embed_resource::NONE);
}
