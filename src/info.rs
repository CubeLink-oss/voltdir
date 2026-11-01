//! `voltdir info`：環境資訊。

use crate::json;
use crate::sys;
use std::io::Write;

pub fn render(tool_version: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let elevated = sys::is_elevated();
    out.extend_from_slice(b"{\"tool_version\":");
    json::push_str(&mut out, tool_version);
    out.extend_from_slice(
        b",\"schema_versions\":{\"snapshot\":[\"voltdir.snapshot/1\"],\"report\":[\"voltdir.report/1\"],\"find\":[\"voltdir.find/1\"],\"diff\":[\"voltdir.diff/1\"]},\"elevated\":",
    );
    out.extend_from_slice(if elevated { b"true" } else { b"false" });
    let drives = sys::logical_drives();
    let any_ntfs = drives.iter().any(|r| {
        sys::volume_details(r).is_some_and(|v| v.fs.eq_ignore_ascii_case("NTFS") && !v.is_network)
    });
    out.extend_from_slice(b",\"engines\":{\"mft\":{\"available\":");
    if elevated && any_ntfs {
        out.extend_from_slice(b"true}");
    } else {
        out.extend_from_slice(b"false,\"reason\":");
        json::push_str(
            &mut out,
            if elevated {
                "no local NTFS volume was found"
            } else {
                "administrator rights are required to read the master file table"
            },
        );
        out.push(b'}');
    }
    out.extend_from_slice(
        b",\"walk\":{\"available\":true}},\"limits\":{\"find_limit_max\":10000,\"top_n_max\":1000,\"threads_max\":4096},\"volumes\":[",
    );
    let mut first = true;
    for root in drives {
        let Some(v) = sys::volume_details(&root) else {
            continue;
        };
        if !first {
            out.push(b',');
        }
        first = false;
        out.extend_from_slice(b"{\"root\":");
        json::push_str_u16(&mut out, &v.root);
        out.extend_from_slice(b",\"fs\":");
        json::push_str(&mut out, &v.fs);
        let _ = write!(
            out,
            ",\"cluster_bytes\":{},\"capacity_bytes\":{},\"free_bytes\":{},\"is_network\":{},\"is_ntfs\":{}}}",
            v.cluster_bytes,
            v.capacity_bytes,
            v.free_bytes,
            v.is_network,
            v.fs.eq_ignore_ascii_case("NTFS")
        );
    }
    out.extend_from_slice(b"]}\n");
    out
}
