// 邦信 · 组件包验签（三端共用）
// ─────────────────────────────────────────────────────────────
// 2026-11-10：wxgo 插件独立发版后，客户端要自己从七牛下载可执行二进制并执行，
// 这是比主程序升级更危险的入口 —— 必须验签，不能只靠 HTTPS。
//
// 公钥与 tauri-plugin-updater / main.rs::verify_portable 是**同一把**
// （keys/x8im.key.pub，keynum 00670EA62A9A6151），签名由发布脚本用
// `tauri signer sign` 产出，写进 latest-wxgo.json 的 signature 字段。
//
// 🔴 两种输入格式都要吃：
//   ① .sig 文件内容 = 4 行签名原文的 base64（tauri 惯例，latest.json 就是这么存的）
//   ② 已解码的 4 行原文（untrusted comment / base64 / trusted comment / global sig）
// 2026-10-25 曾因只认 ② 导致所有 Windows 客户端「下完 38MB 却永远验签失败」。

use base64::Engine as _;
use minisign_verify::{PublicKey, Signature};

const PUBLIC_KEY: &str = "RWRRYZoqpg5nAMR8xmhdtJgSX4BoGUrjrVPDm0D2gvCovnAov9tJjTo8";

pub fn verify(bytes: &[u8], signature: &str) -> Result<(), String> {
    let s = signature.trim();
    if s.is_empty() {
        return Err("组件包签名缺失".into());
    }
    let raw = if s.starts_with("untrusted comment") {
        s.to_string()
    } else {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(|e| format!("签名 base64 解码失败：{e}"))?;
        String::from_utf8(decoded).map_err(|e| format!("签名不是合法 UTF-8：{e}"))?
    };
    let pk = PublicKey::from_base64(PUBLIC_KEY)
        .map_err(|e| format!("公钥解析失败：{e}"))?;
    let sig = Signature::decode(&raw).map_err(|e| format!("签名解析失败：{e}"))?;
    pk.verify(bytes, &sig, false)
        .map_err(|e| format!("组件包验签未通过：{e}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn rejects_empty_signature() {
        assert!(super::verify(b"abc", "").is_err());
    }
    #[test]
    fn rejects_bogus_signature() {
        assert!(super::verify(b"abc", "not-a-signature").is_err());
    }

    /// 真签名回归：用发布脚本同一把私钥（keys/x8im.key）经
    /// `tauri signer sign --app-version test` 对下面这串字节签出的 .sig 内容。
    /// 覆盖「base64 单行 → 4 行原文 → Signature::decode → Ed25519 verify」全链路，
    /// 防止再出现 2026-10-25 那种「下完包却永远验签失败」。
    #[test]
    fn verifies_real_tauri_signature() {
        const SIG: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVSUllab3FwZzVuQU8wN2l1b3JuNUwrODFPeFVpeGpzZUNVUE5FcEhVSGNYN1Z5b1FGbzNPS2pKMExGdk1IT2VrUHV3dm9VOUZ1aG5ZK2lNRWhaOXhpV1cyc2dTNXhqa2dvPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzk0MzE2NjY1CWZpbGU6dmVjdG9yLmJpbgl2ZXJzaW9uOnRlc3QKVk9iQUlWbUpBcXlwc0hsd1l2WDAzSW9WcVFZR01TUHVGSVdHTFFBWlFwanYyV3RXbWEyMmJEYVNLREtRaGUwelVncjBmQ000YUZvYzlSd3FaRFBEREE9PQo=";
        assert!(super::verify(b"bangxin-wxgo-verify-test", SIG).is_ok());
        assert!(super::verify(b"bangxin-wxgo-verify-tampered", SIG).is_err());
    }
}
