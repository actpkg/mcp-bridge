fn main() {
    let info = act_types::ComponentInfo::new(
        "mcp-bridge",
        "0.1.0",
        "Proxies a remote MCP server's tools as ACT tools",
    );
    let mut buf = Vec::new();
    ciborium::into_writer(&info, &mut buf).expect("CBOR encoding failed");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    std::fs::write(format!("{out_dir}/act_component.cbor"), &buf).unwrap();
}
