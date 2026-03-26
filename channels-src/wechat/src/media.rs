use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use aes::Aes128;
use base64::Engine as _;
use md5::{Digest, Md5};
use rand::RngCore;
use serde_json::json;

use crate::exports::near::agent::channel::Attachment;
use crate::near::agent::channel_host::{self, InboundAttachment};
use crate::types::{
    CdnMedia, ImageItem, MessageItem, SendMessageRequest, WechatConfig, MESSAGE_ITEM_IMAGE,
    MESSAGE_STATE_FINISH, MESSAGE_TYPE_BOT, UPLOAD_MEDIA_TYPE_IMAGE,
};

const AES_BLOCK_SIZE: usize = 16;

#[derive(Debug, Clone)]
pub struct UploadImage {
    pub download_encrypted_query_param: String,
    pub aes_key_base64: String,
    pub file_size_ciphertext: u64,
}

pub fn extract_image_attachments(
    config: &WechatConfig,
    message: &crate::types::WechatMessage,
) -> Result<Vec<InboundAttachment>, String> {
    message
        .item_list
        .iter()
        .enumerate()
        .filter_map(|(index, item)| map_image_attachment(config, message, item, index).transpose())
        .collect()
}

pub fn send_image_attachment(
    config: &WechatConfig,
    to_user_id: &str,
    attachment: &Attachment,
    context_token: Option<&str>,
    text: &str,
) -> Result<(), String> {
    if attachment.data.is_empty() {
        return Err(format!(
            "WeChat image attachment '{}' has no data",
            attachment.filename
        ));
    }

    let upload = upload_image(config, to_user_id, attachment)?;
    if !text.trim().is_empty() {
        crate::api::send_text_message(config, to_user_id, text.trim(), context_token)?;
    }
    let request = SendMessageRequest {
        msg: crate::types::OutboundWechatMessage {
            from_user_id: String::new(),
            to_user_id: to_user_id.to_string(),
            client_id: format!("wechat-{}", channel_host::now_millis()),
            message_type: MESSAGE_TYPE_BOT,
            message_state: MESSAGE_STATE_FINISH,
            item_list: vec![MessageItem {
                r#type: Some(MESSAGE_ITEM_IMAGE),
                text_item: None,
                image_item: Some(ImageItem {
                    media: Some(CdnMedia {
                        encrypt_query_param: Some(upload.download_encrypted_query_param.clone()),
                        aes_key: Some(upload.aes_key_base64.clone()),
                        encrypt_type: Some(1),
                    }),
                    aeskey: None,
                    mid_size: Some(upload.file_size_ciphertext),
                }),
            }],
            context_token: context_token.map(str::to_string),
        },
        base_info: crate::api::base_info(),
    };

    crate::api::send_message_request(config, &request)
}

fn map_image_attachment(
    config: &WechatConfig,
    message: &crate::types::WechatMessage,
    item: &MessageItem,
    index: usize,
) -> Result<Option<InboundAttachment>, String> {
    if item.r#type != Some(MESSAGE_ITEM_IMAGE) {
        return Ok(None);
    }

    let image = item.image_item.as_ref().ok_or_else(|| {
        format!(
            "WeChat image message {:?} is missing image_item payload",
            message.message_id
        )
    })?;
    let media = image.media.as_ref().ok_or_else(|| {
        format!(
            "WeChat image message {:?} is missing media payload",
            message.message_id
        )
    })?;
    let encrypt_query_param = media.encrypt_query_param.as_deref().ok_or_else(|| {
        format!(
            "WeChat image message {:?} is missing encrypt_query_param",
            message.message_id
        )
    })?;
    let message_id = message
        .message_id
        .ok_or_else(|| "WeChat image message is missing message_id".to_string())?;
    let aes_key = preferred_image_aes_key(image, media).map(str::to_string);

    Ok(Some(InboundAttachment {
        id: format!("wechat-image-{}-{}", message_id, index),
        mime_type: "image/jpeg".to_string(),
        filename: Some(format!("wechat-image-{}-{}.jpg", message_id, index)),
        size_bytes: image.mid_size,
        source_url: Some(build_cdn_download_url(
            &config.cdn_base_url,
            encrypt_query_param,
        )),
        storage_key: None,
        extracted_text: None,
        extras_json: json!({ "wechat_aes_key": aes_key }).to_string(),
    }))
}

fn preferred_image_aes_key<'a>(image: &'a ImageItem, media: &'a CdnMedia) -> Option<&'a str> {
    image
        .aeskey
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            media
                .aes_key
                .as_deref()
                .filter(|value| !value.trim().is_empty())
        })
}

fn upload_image(
    config: &WechatConfig,
    to_user_id: &str,
    attachment: &Attachment,
) -> Result<UploadImage, String> {
    let plaintext = &attachment.data;
    let raw_size = plaintext.len() as u64;
    let raw_md5 = hex_lower(md5_bytes(plaintext));
    let file_size_ciphertext = padded_size(raw_size);
    let filekey = hex_lower(random_bytes(16)?);
    let aes_key = random_bytes(16)?;
    let aes_key_hex = hex_lower(aes_key.clone());

    let upload_url = crate::api::get_upload_url(
        config,
        &crate::types::GetUploadUrlRequest {
            filekey: filekey.clone(),
            media_type: UPLOAD_MEDIA_TYPE_IMAGE,
            to_user_id: to_user_id.to_string(),
            rawsize: raw_size,
            rawfilemd5: raw_md5,
            filesize: file_size_ciphertext,
            no_need_thumb: true,
            aeskey: aes_key_hex,
            base_info: crate::api::base_info(),
        },
    )?;

    let upload_param = upload_url
        .upload_param
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "WeChat getUploadUrl returned no upload_param".to_string())?;

    if upload_url.thumb_upload_param.is_some() {
        channel_host::log(
            channel_host::LogLevel::Debug,
            "WeChat image upload returned thumb_upload_param; ignoring for single-image flow",
        );
    }

    let ciphertext = encrypt_aes_ecb_pkcs7(plaintext, &aes_key)?;
    let upload_response = channel_host::http_request(
        "POST",
        &build_cdn_upload_url(&config.cdn_base_url, upload_param, &filekey),
        r#"{"Content-Type":"application/octet-stream"}"#,
        Some(&ciphertext),
        Some(15_000),
    )
    .map_err(|e| format!("WeChat CDN upload failed: {e}"))?;

    if upload_response.status != 200 {
        let body = String::from_utf8_lossy(&upload_response.body);
        return Err(format!(
            "WeChat CDN upload returned {}: {}",
            upload_response.status, body
        ));
    }

    let headers: std::collections::HashMap<String, String> =
        serde_json::from_str(&upload_response.headers_json)
            .map_err(|e| format!("Failed to parse WeChat CDN upload headers: {e}"))?;
    let download_encrypted_query_param = headers
        .iter()
        .find_map(|(key, value)| {
            if key.eq_ignore_ascii_case("x-encrypted-param") {
                Some(value.clone())
            } else {
                None
            }
        })
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "WeChat CDN upload response missing x-encrypted-param".to_string())?;

    Ok(UploadImage {
        download_encrypted_query_param,
        aes_key_base64: base64::engine::general_purpose::STANDARD.encode(aes_key),
        file_size_ciphertext,
    })
}

fn build_cdn_download_url(cdn_base_url: &str, encrypted_query_param: &str) -> String {
    format!(
        "{}/download?encrypted_query_param={}",
        cdn_base_url.trim_end_matches('/'),
        percent_encode(encrypted_query_param)
    )
}

fn build_cdn_upload_url(cdn_base_url: &str, upload_param: &str, filekey: &str) -> String {
    format!(
        "{}/upload?encrypted_query_param={}&filekey={}",
        cdn_base_url.trim_end_matches('/'),
        percent_encode(upload_param),
        percent_encode(filekey)
    )
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(nibble_to_hex(byte >> 4));
            encoded.push(nibble_to_hex(byte & 0x0F));
        }
    }
    encoded
}

fn nibble_to_hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + (nibble - 10)) as char,
        _ => '0',
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(nibble_to_hex(byte >> 4));
        out.push(nibble_to_hex(byte & 0x0F));
    }
    out
}

fn hex_lower(bytes: Vec<u8>) -> String {
    encode_hex(&bytes).to_ascii_lowercase()
}

fn encrypt_aes_ecb_pkcs7(plaintext: &[u8], key: &[u8]) -> Result<Vec<u8>, String> {
    let cipher = Aes128::new_from_slice(key).map_err(|e| format!("Invalid AES key: {e}"))?;
    let mut padded = plaintext.to_vec();
    let pad_len = AES_BLOCK_SIZE - (padded.len() % AES_BLOCK_SIZE);
    padded.extend(std::iter::repeat_n(pad_len as u8, pad_len));

    for chunk in padded.chunks_exact_mut(AES_BLOCK_SIZE) {
        cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
    }

    Ok(padded)
}

fn md5_bytes(bytes: &[u8]) -> Vec<u8> {
    Md5::digest(bytes).to_vec()
}

fn random_bytes(len: usize) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0u8; len];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    if bytes.iter().all(|byte| *byte == 0) {
        return Err("OS RNG returned all-zero bytes unexpectedly".to_string());
    }
    Ok(bytes)
}

fn padded_size(raw_size: u64) -> u64 {
    ((raw_size / AES_BLOCK_SIZE as u64) + 1) * AES_BLOCK_SIZE as u64
}

#[cfg(test)]
mod tests {
    use super::{encode_hex, encrypt_aes_ecb_pkcs7, map_image_attachment, AES_BLOCK_SIZE};
    use crate::types::{
        CdnMedia, ImageItem, MessageItem, WechatConfig, WechatMessage, MESSAGE_ITEM_IMAGE,
    };

    #[test]
    fn test_encrypt_aes_ecb_pkcs7_is_block_aligned() {
        let key = [0x11u8; 16];
        let plaintext = b"wechat image payload".to_vec();
        let ciphertext = encrypt_aes_ecb_pkcs7(&plaintext, &key).unwrap();
        assert_eq!(ciphertext.len() % AES_BLOCK_SIZE, 0);
        assert_ne!(ciphertext, plaintext);
        assert_eq!(
            encode_hex(&ciphertext).to_ascii_lowercase(),
            "a7464c94a03fb2c5aa783597a1d2f5a461f1cd5d83a7bd92721e8ac1853f881f"
        );
    }

    #[test]
    fn test_map_image_attachment_errors_when_message_id_missing() {
        let config = WechatConfig::default();
        let message = WechatMessage {
            message_id: None,
            from_user_id: Some("user-1".to_string()),
            to_user_id: Some("bot-1".to_string()),
            session_id: None,
            message_type: None,
            context_token: None,
            item_list: vec![MessageItem {
                r#type: Some(MESSAGE_ITEM_IMAGE),
                text_item: None,
                image_item: Some(ImageItem {
                    media: Some(CdnMedia {
                        encrypt_query_param: Some("enc".to_string()),
                        aes_key: Some("aes".to_string()),
                        encrypt_type: Some(1),
                    }),
                    aeskey: None,
                    mid_size: Some(128),
                }),
            }],
        };

        let error = map_image_attachment(&config, &message, &message.item_list[0], 0)
            .expect_err("missing message_id should error");
        assert!(error.contains("missing message_id"));
    }
}
