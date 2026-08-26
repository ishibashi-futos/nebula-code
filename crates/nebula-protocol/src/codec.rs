use crate::error::ProtocolError;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// 1 フレームの上限。これを超える要求は壊れた接続とみなして切断する。
///
/// 大きなファイルの内容はフレームに載るため、実用上のファイルサイズ上限も兼ねる。
pub const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

const HEADER_BYTES: usize = 4;

/// メッセージを「4 バイト長 + MessagePack」のフレームに符号化する。
pub fn encode_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtocolError> {
    let payload = rmp_serde::to_vec_named(message)
        .map_err(|e| ProtocolError::internal(format!("直列化に失敗しました: {e}")))?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::invalid(format!(
            "フレームが上限を超えました: {} バイト",
            payload.len()
        )));
    }
    let mut frame = Vec::with_capacity(HEADER_BYTES + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

/// ストリームから読み出したバイト列を蓄積し、完成したフレームを取り出すデコーダ。
///
/// トランスポート非依存にしてあるので、GUI 側 (gpui のバックグラウンド executor) と
/// バックエンド側 (tokio) の双方から同じ実装を使える。
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
    /// 読み出し済みバイト数。`buffer` の先頭詰め替えを毎回やらないためのカーソル。
    cursor: usize,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// ストリームから読んだ生バイトを投入する。
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// 完成したフレームが 1 つあれば復号して返す。
    ///
    /// 呼び出し側は `None` が返るまで繰り返し呼ぶ。1 回の `feed` で複数フレームが
    /// 到着することがあるため、ループが必要。
    pub fn next_message<T: DeserializeOwned>(&mut self) -> Result<Option<T>, ProtocolError> {
        self.compact();
        let available = &self.buffer[self.cursor..];
        if available.len() < HEADER_BYTES {
            return Ok(None);
        }
        let len =
            u32::from_be_bytes([available[0], available[1], available[2], available[3]]) as usize;
        if len > MAX_FRAME_BYTES {
            return Err(ProtocolError::invalid(format!(
                "フレーム長が不正です: {len} バイト"
            )));
        }
        if available.len() < HEADER_BYTES + len {
            return Ok(None);
        }
        let payload = &available[HEADER_BYTES..HEADER_BYTES + len];
        let message = rmp_serde::from_slice(payload)
            .map_err(|e| ProtocolError::invalid(format!("復号に失敗しました: {e}")))?;
        self.cursor += HEADER_BYTES + len;
        Ok(Some(message))
    }

    /// 読み終えた前半を捨てる。
    ///
    /// 毎回 `drain` すると O(n^2) になるため、未読部分より読み終えた部分が大きくなった
    /// ときだけ詰め替える。
    fn compact(&mut self) {
        if self.cursor == 0 {
            return;
        }
        if self.cursor >= self.buffer.len() {
            self.buffer.clear();
            self.cursor = 0;
        } else if self.cursor > self.buffer.len() - self.cursor {
            self.buffer.drain(..self.cursor);
            self.cursor = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Sample {
        n: u32,
        s: String,
    }

    fn sample(n: u32) -> Sample {
        Sample {
            n,
            s: format!("値 {n}"),
        }
    }

    #[test]
    fn 単一フレームを往復できる() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(&encode_frame(&sample(7)).unwrap());
        assert_eq!(decoder.next_message::<Sample>().unwrap(), Some(sample(7)));
        assert_eq!(decoder.next_message::<Sample>().unwrap(), None);
    }

    #[test]
    fn 一度に届いた複数フレームを順に取り出せる() {
        let mut decoder = FrameDecoder::new();
        let mut bytes = Vec::new();
        for n in 0..5 {
            bytes.extend_from_slice(&encode_frame(&sample(n)).unwrap());
        }
        decoder.feed(&bytes);
        for n in 0..5 {
            assert_eq!(decoder.next_message::<Sample>().unwrap(), Some(sample(n)));
        }
        assert_eq!(decoder.next_message::<Sample>().unwrap(), None);
    }

    #[test]
    fn バイト単位に分割されても復元できる() {
        let mut decoder = FrameDecoder::new();
        let bytes = encode_frame(&sample(42)).unwrap();
        for (i, byte) in bytes.iter().enumerate() {
            decoder.feed(&[*byte]);
            let decoded = decoder.next_message::<Sample>().unwrap();
            if i + 1 == bytes.len() {
                assert_eq!(decoded, Some(sample(42)));
            } else {
                assert_eq!(decoded, None, "{i} バイト目で早すぎる復号が起きた");
            }
        }
    }

    #[test]
    fn 長さが上限を超えるヘッダは拒否する() {
        let mut decoder = FrameDecoder::new();
        decoder.feed(&u32::MAX.to_be_bytes());
        assert!(decoder.next_message::<Sample>().is_err());
    }

    #[test]
    fn 大量のフレームを流してもバッファが単調増加しない() {
        let mut decoder = FrameDecoder::new();
        for n in 0..10_000 {
            decoder.feed(&encode_frame(&sample(n)).unwrap());
            assert!(decoder.next_message::<Sample>().unwrap().is_some());
        }
        assert!(
            decoder.buffer.len() < 4096,
            "バッファが肥大化している: {}",
            decoder.buffer.len()
        );
    }
}
