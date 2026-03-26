use std::time::Duration;

use aes::Aes128;
use aes::cipher::{BlockDecrypt, KeyInit, generic_array::GenericArray};
use base64::Engine as _;
use serde::Deserialize;

use crate::channels::wasm::capabilities::ChannelCapabilities;
use crate::channels::wasm::host::{Attachment, ChannelHostState};

const AES_BLOCK_SIZE: usize = 16;
const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;
const WECHAT_CHANNEL_NAME: &str = "wechat";

#[derive(Debug, Deserialize)]
struct WechatAttachmentExtras {
    #[serde(default)]
    wechat_aes_key: Option<String>,
}

pub(crate) async fn hydrate_attachment_for_channel(
    channel_name: &str,
    capabilities: &ChannelCapabilities,
    attachment: &mut Attachment,
) {
    if !should_hydrate_wechat_attachment(channel_name, attachment) {
        return;
    }

    let Some(source_url) = attachment.source_url.as_deref() else {
        return;
    };
    let Some(encoded_aes_key) = wechat_aes_key(&attachment.extras_json) else {
        tracing::warn!(
            channel = %channel_name,
            attachment_id = %attachment.id,
            "Skipping WeChat image hydration: missing AES key metadata"
        );
        return;
    };

    match download_wechat_attachment_bytes(channel_name, capabilities, source_url).await {
        Ok(ciphertext) => match decrypt_wechat_image_bytes(&ciphertext, &encoded_aes_key) {
            Ok(plaintext) => {
                attachment.size_bytes = Some(plaintext.len() as u64);
                attachment.mime_type = detect_image_mime(&plaintext).to_string();
                attachment.data = plaintext;
            }
            Err(error) => {
                tracing::warn!(
                    channel = %channel_name,
                    attachment_id = %attachment.id,
                    error = %error,
                    "Failed to decrypt WeChat image attachment"
                );
            }
        },
        Err(error) => {
            tracing::warn!(
                channel = %channel_name,
                attachment_id = %attachment.id,
                error = %error,
                "Failed to download WeChat image attachment"
            );
        }
    }
}

fn should_hydrate_wechat_attachment(channel_name: &str, attachment: &Attachment) -> bool {
    channel_name == WECHAT_CHANNEL_NAME
        && attachment.data.is_empty()
        && attachment.mime_type.starts_with("image/")
}

fn wechat_aes_key(extras_json: &str) -> Option<String> {
    if extras_json.trim().is_empty() {
        return None;
    }

    serde_json::from_str::<WechatAttachmentExtras>(extras_json)
        .ok()
        .and_then(|extras| extras.wechat_aes_key)
        .filter(|value| !value.trim().is_empty())
}

async fn download_wechat_attachment_bytes(
    channel_name: &str,
    capabilities: &ChannelCapabilities,
    source_url: &str,
) -> Result<Vec<u8>, String> {
    let host_state = ChannelHostState::new(channel_name, capabilities.clone());
    host_state.check_http_allowed(source_url, "GET")?;

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let response = client
        .get(source_url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("WeChat CDN download failed: {e}"))?;

    if response.status() != reqwest::StatusCode::OK {
        return Err(format!(
            "WeChat CDN download returned {}",
            response.status()
        ));
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Failed to read WeChat CDN response body: {e}"))?
        .to_vec();

    if bytes.is_empty() {
        return Err("WeChat CDN download returned an empty body".to_string());
    }
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "WeChat image attachment exceeds {MAX_ATTACHMENT_BYTES} bytes"
        ));
    }

    Ok(bytes)
}

fn decrypt_wechat_image_bytes(ciphertext: &[u8], encoded_aes_key: &str) -> Result<Vec<u8>, String> {
    let key = parse_aes_key(encoded_aes_key)?;
    decrypt_aes_ecb_pkcs7(ciphertext, &key)
}

fn parse_aes_key(encoded: &str) -> Result<Vec<u8>, String> {
    let decoded = if encoded.len() == 32 && encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        decode_hex(encoded)?
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|e| format!("Failed to decode WeChat AES key: {e}"))?
    };

    if decoded.len() == AES_BLOCK_SIZE {
        return Ok(decoded);
    }

    if decoded.len() == 32 && decoded.iter().all(|byte| byte.is_ascii_hexdigit()) {
        return decode_hex(
            std::str::from_utf8(&decoded)
                .map_err(|e| format!("WeChat AES key hex payload is not valid UTF-8: {e}"))?,
        );
    }

    Err(format!(
        "WeChat AES key must decode to 16 bytes or a 32-char hex string, got {} bytes",
        decoded.len()
    ))
}

fn decode_hex(input: &str) -> Result<Vec<u8>, String> {
    if !input.len().is_multiple_of(2) {
        return Err("hex input length must be even".to_string());
    }
    let mut bytes = Vec::with_capacity(input.len() / 2);
    let chars: Vec<u8> = input.as_bytes().to_vec();
    for idx in (0..chars.len()).step_by(2) {
        let high = from_hex_digit(chars[idx])?;
        let low = from_hex_digit(chars[idx + 1])?;
        bytes.push((high << 4) | low);
    }
    Ok(bytes)
}

fn from_hex_digit(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(format!("invalid hex digit '{}'", value as char)),
    }
}

fn decrypt_aes_ecb_pkcs7(ciphertext: &[u8], key: &[u8]) -> Result<Vec<u8>, String> {
    if !ciphertext.len().is_multiple_of(AES_BLOCK_SIZE) {
        return Err("ciphertext length is not a multiple of 16 bytes".to_string());
    }

    let cipher = Aes128::new_from_slice(key).map_err(|e| format!("Invalid AES key: {e}"))?;
    let mut plaintext = ciphertext.to_vec();
    for chunk in plaintext.chunks_exact_mut(AES_BLOCK_SIZE) {
        cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
    }

    let pad_len = *plaintext
        .last()
        .ok_or_else(|| "ciphertext decrypted to an empty buffer".to_string())?
        as usize;
    if pad_len == 0 || pad_len > AES_BLOCK_SIZE || pad_len > plaintext.len() {
        return Err("invalid PKCS7 padding".to_string());
    }
    if !plaintext[plaintext.len() - pad_len..]
        .iter()
        .all(|byte| *byte as usize == pad_len)
    {
        return Err("invalid PKCS7 padding bytes".to_string());
    }
    plaintext.truncate(plaintext.len() - pad_len);
    Ok(plaintext)
}

fn detect_image_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "image/gif"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

#[cfg(test)]
fn encrypt_aes_ecb_pkcs7(plaintext: &[u8], key: &[u8]) -> Result<Vec<u8>, String> {
    use aes::cipher::BlockEncrypt;

    let cipher = Aes128::new_from_slice(key).map_err(|e| format!("Invalid AES key: {e}"))?;
    let mut padded = plaintext.to_vec();
    let pad_len = AES_BLOCK_SIZE - (padded.len() % AES_BLOCK_SIZE);
    padded.extend(std::iter::repeat_n(pad_len as u8, pad_len));

    for chunk in padded.chunks_exact_mut(AES_BLOCK_SIZE) {
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
    }

    Ok(padded)
}

#[cfg(test)]
mod tests {
    use super::{
        Attachment, decrypt_wechat_image_bytes, detect_image_mime, encrypt_aes_ecb_pkcs7,
        hydrate_attachment_for_channel, should_hydrate_wechat_attachment,
    };
    use crate::channels::wasm::ChannelCapabilities;
    use base64::Engine as _;

    fn make_attachment() -> Attachment {
        Attachment {
            id: "wechat-image-1".to_string(),
            mime_type: "image/jpeg".to_string(),
            filename: Some("wechat-image.jpg".to_string()),
            size_bytes: None,
            source_url: Some(
                "https://novac2c.cdn.weixin.qq.com/c2c/download?encrypted_query_param=test"
                    .to_string(),
            ),
            storage_key: None,
            extracted_text: None,
            extras_json: String::new(),
            data: Vec::new(),
            duration_secs: None,
        }
    }

    fn encode_test_extras_json(aes_key: &str) -> String {
        serde_json::json!({ "wechat_aes_key": aes_key }).to_string()
    }

    #[test]
    fn decrypt_wechat_image_bytes_round_trips() {
        let key = [7u8; 16];
        let plaintext = vec![0xFF, 0xD8, 0xFF, 0xDB, 0x00, 0x11];
        let ciphertext = encrypt_aes_ecb_pkcs7(&plaintext, &key).unwrap();
        let encoded_key = base64::engine::general_purpose::STANDARD.encode(key);
        let decrypted = decrypt_wechat_image_bytes(&ciphertext, &encoded_key).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn detect_image_mime_prefers_magic_bytes() {
        assert_eq!(detect_image_mime(&[0xFF, 0xD8, 0xFF, 0x00]), "image/jpeg");
        assert_eq!(
            detect_image_mime(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            "image/png"
        );
    }

    #[test]
    fn wechat_attachment_hydration_only_applies_to_wechat_images() {
        let mut attachment = make_attachment();
        attachment.extras_json = encode_test_extras_json("ZmFrZS1rZXk=");
        assert!(should_hydrate_wechat_attachment("wechat", &attachment));
        assert!(!should_hydrate_wechat_attachment("telegram", &attachment));

        attachment.mime_type = "application/pdf".to_string();
        assert!(!should_hydrate_wechat_attachment("wechat", &attachment));
    }

    #[tokio::test]
    async fn hydration_skips_when_metadata_is_missing() {
        let mut attachment = make_attachment();
        let caps = ChannelCapabilities::for_channel("wechat");
        hydrate_attachment_for_channel("wechat", &caps, &mut attachment).await;
        assert!(attachment.data.is_empty());
        assert_eq!(attachment.size_bytes, None);
    }
}
