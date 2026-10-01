use bytes::{BufMut, BytesMut};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum Modbus TCP ADU size: 7-byte MBAP + up to 253 PDU bytes (excl. unit id in length).
/// Length field counts unit_id + PDU, max 255 → total frame = 6 + length ≤ 261.
pub const MAX_ADU_LEN: usize = 260;
pub const MBAP_HEADER_LEN: usize = 7;

#[derive(Debug, Error)]
pub enum MbapError {
    #[error("connection closed")]
    Closed,
    #[error("invalid protocol id {0} (expected 0)")]
    InvalidProtocolId(u16),
    #[error("invalid length field {0}")]
    InvalidLength(u16),
    #[error("frame too large ({0} bytes)")]
    TooLarge(usize),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Read one complete Modbus TCP ADU (MBAP header + PDU) from `reader`.
pub async fn read_adu<R: AsyncRead + Unpin>(reader: &mut R) -> Result<BytesMut, MbapError> {
    let mut header = [0u8; MBAP_HEADER_LEN];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Err(MbapError::Closed);
        }
        Err(e) => return Err(MbapError::Io(e)),
    }

    let protocol_id = u16::from_be_bytes([header[2], header[3]]);
    if protocol_id != 0 {
        return Err(MbapError::InvalidProtocolId(protocol_id));
    }

    let length = u16::from_be_bytes([header[4], header[5]]);
    // length includes unit_id (1) + PDU; must be at least 2 (unit_id + function code)
    if length < 2 {
        return Err(MbapError::InvalidLength(length));
    }

    let pdu_len = (length as usize).saturating_sub(1); // remaining after unit_id already in header
    let total = MBAP_HEADER_LEN + pdu_len;
    if total > MAX_ADU_LEN {
        return Err(MbapError::TooLarge(total));
    }

    let mut frame = BytesMut::with_capacity(total);
    frame.put_slice(&header);

    if pdu_len > 0 {
        let start = frame.len();
        frame.resize(total, 0);
        match reader.read_exact(&mut frame[start..]).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(MbapError::Closed);
            }
            Err(e) => return Err(MbapError::Io(e)),
        }
    }

    Ok(frame)
}

/// Write a complete ADU to `writer` and flush.
pub async fn write_adu<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &[u8],
) -> Result<(), MbapError> {
    writer.write_all(frame).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn reads_valid_adu() {
        // Read holding registers request: tid=1, proto=0, len=6, unit=1, fc=3, addr=0, qty=1
        let data: &[u8] = &[
            0x00, 0x01, 0x00, 0x00, 0x00, 0x06, 0x01, 0x03, 0x00, 0x00, 0x00, 0x01,
        ];
        let mut cursor = data;
        let frame = read_adu(&mut cursor).await.unwrap();
        assert_eq!(&frame[..], data);
        assert_eq!(cursor.read(&mut [0u8; 1]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn rejects_bad_protocol_id() {
        let data: &[u8] = &[
            0x00, 0x01, 0x00, 0x01, 0x00, 0x06, 0x01, 0x03, 0x00, 0x00, 0x00, 0x01,
        ];
        let mut cursor = data;
        let err = read_adu(&mut cursor).await.unwrap_err();
        assert!(matches!(err, MbapError::InvalidProtocolId(1)));
    }
}
