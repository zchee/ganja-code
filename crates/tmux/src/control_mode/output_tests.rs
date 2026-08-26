use super::*;

#[test]
fn carriage_return_and_newline_decode() {
    assert_eq!(
        decode_output_value(r"hello\015\012").unwrap(),
        b"hello\r\n".to_vec()
    );
}

#[test]
fn escaped_backslash_decodes() {
    assert_eq!(
        decode_output_value(r"path\134name").unwrap(),
        b"path\\name".to_vec()
    );
}

#[test]
fn terminal_escape_bytes_decode() {
    assert_eq!(
        decode_output_value(r"\033[31mred\033[0m").unwrap(),
        b"\x1b[31mred\x1b[0m".to_vec()
    );
}

#[test]
fn non_utf8_bytes_are_preserved() {
    assert_eq!(
        decode_output_value(r"bin\377ary").unwrap(),
        vec![b'b', b'i', b'n', 0xff, b'a', b'r', b'y']
    );
}

#[test]
fn large_payload_decodes() {
    let mut value = "a".repeat(8192);
    value.push_str(r"\012");
    let mut want = vec![b'a'; 8192];
    want.push(b'\n');
    assert_eq!(decode_output_value(&value).unwrap(), want);
}

#[test]
fn incomplete_escape_is_rejected() {
    let err = decode_output_value(r"bad\01").unwrap_err();
    assert!(err.to_string().contains("incomplete octal escape"));
}

#[test]
fn invalid_digit_is_rejected() {
    let err = decode_output_value(r"bad\09x").unwrap_err();
    assert!(err.to_string().contains("invalid octal digit"));
}

#[test]
fn out_of_range_escape_is_rejected() {
    let err = decode_output_value(r"bad\777").unwrap_err();
    assert!(err.to_string().contains("out of range"));
}

// Go's TestOutputNotificationText and
// TestOutputNotificationTextLossyKeepsPartialDecode drive
// decodeOutputText/decodeOutputTextLossy through the OutputNotification
// wrapper type, which lives in the `notification` module. The literal
// ports through that wrapper sit beside the wrapper in `notification`'s
// tests; the two tests below keep the same cases on the bare decode
// functions, where the decoding logic itself lives.

#[test]
fn output_text_decodes_or_rejects_invalid_utf8() {
    assert_eq!(decode_output_text(r"hello\012").unwrap(), "hello\n");
    let err = decode_output_text(r"bad\377").unwrap_err();
    assert!(err.to_string().contains("valid UTF-8"));
}

#[test]
fn output_text_lossy_keeps_partial_decode() {
    assert_eq!(decode_output_text_lossy(r"ok\01"), "ok");
}
