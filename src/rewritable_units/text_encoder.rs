use super::ContentType;
use crate::html::escape_body_text;
use encoding_rs::{CoderResult, Decoder, DecoderResult, Encoder, Encoding, UTF_8};
use thiserror::Error;

/// Input contained non-UTF-8 byte sequence
///
/// [`StreamingHandlerSink::write_utf8_chunk`] will not fail on an incomplete UTF-8 sequence at the end of the chunk,
/// but it will report errors if incomplete UTF-8 sequences are within the chunk, or the next call starts with
/// bytes that don't match the previous call's trailing bytes.
#[derive(Error, Debug, Eq, PartialEq, Copy, Clone)]
#[error("Invalid UTF-8")]
pub struct Utf8Error;

/// Used to write chunks of text or markup in streaming mutation handlers.
///
/// Argument to [`StreamingHandler::write_all()`](crate::html_content::StreamingHandler::write_all).
pub struct StreamingHandlerSink<'output_handler> {
    incomplete_utf8: Option<IncompleteUtf8Resync>,
    inner: StreamingHandlerSinkInner<'output_handler>,
}

struct StreamingHandlerSinkInner<'output_handler> {
    non_utf8_encoder: Option<TextEncoder>,

    /// ```compile_fail
    /// use lol_html::html_content::StreamingHandlerSink;
    /// struct IsSend<T: Send>(T);
    /// let x: IsSend<StreamingHandlerSink<'static>>;
    /// ```
    ///
    /// ```compile_fail
    /// use lol_html::html_content::StreamingHandlerSink;
    /// struct IsSync<T: Sync>(T);
    /// let x: IsSync<StreamingHandlerSink<'static>>;
    /// ```
    output_handler: &'output_handler mut dyn FnMut(&[u8]),
}

impl<'output_handler> StreamingHandlerSink<'output_handler> {
    #[inline(always)]
    pub(crate) fn new(
        encoding: &'static Encoding,
        output_handler: &'output_handler mut dyn FnMut(&[u8]),
    ) -> Self {
        Self {
            incomplete_utf8: None,
            inner: StreamingHandlerSinkInner {
                non_utf8_encoder: (encoding != UTF_8).then(|| TextEncoder::new(encoding)),
                output_handler,
            },
        }
    }

    /// Writes the given UTF-8 string to the output, converting the encoding and [escaping](ContentType) if necessary.
    ///
    /// It may be called multiple times. The strings will be concatenated together.
    #[inline]
    pub fn write_str(&mut self, content: &str, content_type: ContentType) {
        if self
            .incomplete_utf8
            .as_mut()
            .is_some_and(|d| d.discard_incomplete())
        {
            // too late to report the error to the caller of write_utf8_chunk
            self.inner.write_html("\u{FFFD}");
        }
        self.inner.write_str(content, content_type);
    }

    #[inline]
    pub(crate) fn output_handler(&mut self) -> &mut dyn FnMut(&[u8]) {
        &mut self.inner.output_handler
    }

    /// Writes as much of the given UTF-8 fragment as possible, converting the encoding and [escaping](ContentType) if necessary.
    ///
    /// The `content` doesn't need to be a complete UTF-8 string, as long as consecutive calls to `write_utf8_bytes` create a valid UTF-8 string.
    /// Any incomplete UTF-8 sequence at the end of the content is buffered and flushed as soon as it's completed.
    ///
    /// Other methods like `write_str_chunk` should not be called after a `write_utf8_bytes` call with an incomplete UTF-8 sequence.
    #[inline]
    pub fn write_utf8_chunk(
        &mut self,
        mut content: &[u8],
        content_type: ContentType,
    ) -> Result<(), Utf8Error> {
        let decoder = self
            .incomplete_utf8
            .get_or_insert_with(IncompleteUtf8Resync::new);
        while !content.is_empty() {
            let (valid_chunk, rest) = decoder.utf8_bytes_to_slice(content, false)?;
            content = rest;
            if !valid_chunk.is_empty() {
                self.inner.write_str(valid_chunk, content_type);
            }
        }
        Ok(())
    }
}

impl<'output_handler> StreamingHandlerSinkInner<'output_handler> {
    #[inline]
    pub(crate) fn write_str(&mut self, content: &str, content_type: ContentType) {
        match content_type {
            ContentType::Html => self.write_html(content),
            ContentType::Text => self.write_body_text(content),
        }
    }

    pub(crate) fn write_html(&mut self, html: &str) {
        if let Some(encoder) = &mut self.non_utf8_encoder {
            encoder.encode(html, self.output_handler);
        } else if !html.is_empty() {
            (self.output_handler)(html.as_bytes());
        }
    }

    /// For text content, not attributes
    pub(crate) fn write_body_text(&mut self, plaintext: &str) {
        if let Some(encoder) = &mut self.non_utf8_encoder {
            escape_body_text(plaintext, &mut |chunk| {
                debug_assert!(!chunk.is_empty());
                encoder.encode(chunk, self.output_handler);
            });
        } else {
            escape_body_text(plaintext, &mut |chunk| {
                debug_assert!(!chunk.is_empty());
                (self.output_handler)(chunk.as_bytes());
            });
        }
    }
}

enum Buffer {
    Heap(Vec<u8>),
    Stack([u8; 63]), // leave a byte for the tag
}

struct TextEncoder {
    encoder: Encoder,
    buffer: Buffer,
}

impl TextEncoder {
    #[inline]
    pub fn new(encoding: &'static Encoding) -> Self {
        debug_assert!(encoding != UTF_8);
        debug_assert!(encoding.is_ascii_compatible());
        Self {
            encoder: encoding.new_encoder(),
            buffer: Buffer::Stack([0; 63]),
        }
    }

    /// This is more efficient than `Bytes::from_str`, because it can output non-UTF-8/non-ASCII encodings
    /// without heap allocations.
    /// It also avoids methods that have UB: https://github.com/hsivonen/encoding_rs/issues/79
    #[inline(never)]
    fn encode(&mut self, mut content: &str, output_handler: &mut dyn FnMut(&[u8])) {
        loop {
            debug_assert!(!self.encoder.has_pending_state()); // ASCII-compatible encodings are not supposed to have it
            let ascii_len = Encoding::ascii_valid_up_to(content.as_bytes());
            if let Some((ascii, remainder)) = content.split_at_checked(ascii_len) {
                if !ascii.is_empty() {
                    (output_handler)(ascii.as_bytes());
                }
                if remainder.is_empty() {
                    return;
                }
                content = remainder;
            }

            let buffer = match &mut self.buffer {
                Buffer::Heap(buf) => buf.as_mut_slice(),
                // Long non-ASCII content could take lots of roundtrips through the encoder
                buf if content.len() >= 1 << 20 => {
                    *buf = Buffer::Heap(vec![0; 4096]);
                    match buf {
                        Buffer::Heap(buf) => buf.as_mut(),
                        _ => unreachable!(),
                    }
                }
                Buffer::Stack(buf) => buf.as_mut_slice(),
            };

            let (result, read, written, _) = self.encoder.encode_from_utf8(content, buffer, false);
            if written > 0 && written <= buffer.len() {
                (output_handler)(&buffer[..written]);
            }
            if read >= content.len() {
                return;
            }
            content = &content[read..];
            match result {
                CoderResult::InputEmpty => {
                    debug_assert!(content.is_empty());
                    return;
                }
                CoderResult::OutputFull => {
                    match &mut self.buffer {
                        Buffer::Heap(buf) if buf.len() >= 1024 => {
                            if written == 0 {
                                panic!("encoding_rs infinite loop"); // encoding_rs only needs a dozen bytes
                            }
                        }
                        buf => *buf = Buffer::Heap(vec![0; 1024]),
                    }
                }
            }
        }
    }
}

struct IncompleteUtf8Resync {
    decoder: Decoder,
    buffer: String,
}

impl IncompleteUtf8Resync {
    pub fn new() -> Self {
        Self {
            decoder: UTF_8.new_decoder_without_bom_handling(),
            buffer: "\0".repeat(1024),
        }
    }

    pub fn utf8_bytes_to_slice<'buf, 'src: 'buf>(
        &'buf mut self,
        content: &'src [u8],
        is_last: bool,
    ) -> Result<(&'buf str, &'src [u8]), Utf8Error> {
        let (result, read, written) =
            self.decoder
                .decode_to_str_without_replacement(content, &mut self.buffer, is_last);

        match result {
            DecoderResult::InputEmpty => {}
            DecoderResult::OutputFull => {
                if written == 0 {
                    panic!("encoding_rs infinite loop"); // the buffer is always large enough
                }
            }
            DecoderResult::Malformed(_, _) => return Err(Utf8Error),
        }

        let written = &self.buffer[..written];
        let remaining = &content[read..];
        Ok((written, remaining))
    }

    /// True if there were incomplete invalid bytes in the buffer
    pub fn discard_incomplete(&mut self) -> bool {
        match self.utf8_bytes_to_slice(b"", true) {
            Ok((valid_chunk, rest)) => {
                debug_assert!(rest.is_empty());
                debug_assert!(valid_chunk.is_empty()); // this can't happen in UTF-8 after empty write
                false
            }
            Err(_) => true,
        }
    }
}

#[test]
fn utf8_fragments() {
    let text = "🐈°文字化けしない ▀▄ ɯopuɐɹ ⓤⓝⓘⓒⓞⓓⓔ and ascii 🐳 sʇuıodǝpoɔ ✴";
    for with_zero_writes in [false, true] {
        for len in 1..9 {
            let mut out = Vec::new();
            let mut handler = |ch: &[u8]| out.extend_from_slice(ch);
            let mut t = StreamingHandlerSink::new(UTF_8, &mut handler);
            for (nth, chunk) in text.as_bytes().chunks(len).enumerate() {
                let msg =
                    format!("{len} at {nth} '{chunk:?}'; with_zero_writes={with_zero_writes}");
                if with_zero_writes {
                    t.write_utf8_chunk(b"", ContentType::Text).expect(&msg);
                }
                t.write_utf8_chunk(chunk, ContentType::Html).expect(&msg);
            }
            drop(t);
            assert_eq!(String::from_utf8_lossy(&out), text, "{len}");
        }
    }
}

#[test]
fn long_text() {
    let mut written = 0;
    let mut expected = 0;
    let mut handler = |ch: &[u8]| {
        assert!(
            ch.iter().all(|&c| {
                written += 1;
                c == if 0 != written & 1 {
                    177
                } else {
                    b'0' + ((written / 2 - 1) % 10) as u8
                }
            }),
            "@{written} {ch:?}"
        );
    };
    let mut t = StreamingHandlerSink::new(encoding_rs::ISO_8859_2, &mut handler);

    let mut s = "ą0ą1ą2ą3ą4ą5ą6ą7ą8ą9".repeat(128);
    let mut split_point = 1;
    while s.len() <= 1 << 17 {
        s.push_str(&s.clone());
        expected += s.chars().count();
        let (a, b) = s.as_bytes().split_at(split_point);
        split_point += 13;
        t.write_utf8_chunk(a, ContentType::Text).unwrap();
        t.write_utf8_chunk(b, ContentType::Html).unwrap();
    }
    assert_eq!(expected, written);
}

#[test]
fn invalid_utf8_fragments() {
    #[rustfmt::skip]
    let broken_utf8 = &[
        &b"\x31\x32\x33\xED\xA0\x80\x31"[..], b"\x31\x32\x33\xEF\x80", b"\x31\x32\x33\xEF\x80\xF0\x3c",
         b"\x37\x38\x39\xFE", b"\x37\x38\xFE", b"\x37\xFF", b"\x3c\x23\x24\xFE\x3C", b"\x3C\x23\xFE\x3C\x3C",
         b"\x3C\x3D\xE0\x80\x3C", b"\x3C\x3D\xE0\x80\xAF\x3C", b"\x3C\x3D\xE0\x80\xE0\x80\x3C",
         b"\x3C\x3D\xED\xA0\x80\x3C", b"\x3C\x3D\xF0\x80\x80\x3C", b"\x3C\x3D\xF0\x80\x80\x80\x3C",
         b"\x3C\x3D\xF7\xBF\xBF\xBF\x3C", b"\x3C\x3D\xFF\x3C", b"\x7F", b"\x80", b"\x80\x3C",
         b"\x80\x81\x82\x83\x84\x85\x86\x87", b"\x80\xBF", b"\x80\xBF\x80", b"\x80\xBF\x80\xBF",
         b"\x80\xBF\x80\xBF\x80", b"\x80\xBF\x80\xBF\x80\xBF", b"\x81", b"\x81\x3C",
         b"\x88\x89\x8A\x8B\x8C\x8D\x8E\x8F", b"\x90\x91\x92\x93\x94\x95\x96\x97", b"\x98\x99\x9A\x9B\x9C\x9D\x9E\x9F",
         b"\xA0\xA1\xA2\xA3\xA4\xA5\xA6\xA7", b"\xA8\xA9\xAA\xAB\xAC\xAD\xAE\xAF", b"\xB0\xB1\xB2\xB3\xB4\xB5\xB6\xB7",
         b"\xB8\xB9\xBA\xBB\xBC\xBD\xBE\xBF", b"\xBF", b"\xC0", b"\xC0\x3C\xC1\x3C\xC2\x3C\xC3\x3C", b"\xC0\x80",
         b"\xC0\xAF", b"\xC0\xAF\xE0\x80\xBF\xF0\x81\x82\x41", b"\xC1\x3C", b"\xC1\xBF", b"\xC1\xBF", b"\xC2\x00",
         b"\xC2\x41\x42", b"\xC2\x7F", b"\xC2\xC0", b"\xC2\xFF", b"\xC4\x3C\xC5\x3C\xC6\x3C\xC7\x3C",
         b"\xC8\x3C\xC9\x3C\xCA\x3C\xCB\x3C", b"\xCC\x3C\xCD\x3C\xCE\x3C\xCF\x3C", b"\xD0\x3C\xD1\x3C\xD2\x3C\xD3\x3C",
         b"\xD4\x3C\xD5\x3C\xD6\x3C\xD7\x3C", b"\xD8\x3C\xD9\x3C\xDA\x3C\xDB\x3C", b"\xDC\x3C\xDD\x3C\xDE\x3C\xDF\x3C",
         b"\xDF", b"\xDF\x00", b"\xDF\x7F", b"\xDF\xC0", b"\xDF\xFF", b"\xE0\x3C\xE1\x3C\xE2\x3C\xE3\x3C", b"\xE0\x80",
         b"\xE0\x80\x00", b"\xE0\x80\x7F", b"\xE0\x80\x80", b"\xE0\x80\xAF", b"\xE0\x80\xC0", b"\xE0\x80\xFF",
         b"\xE0\x81\xBF", b"\xE0\x9F\xBF", b"\xE1\x80\xE2\xF0\x91\x92\xF1\xBF\x41",
         b"\xE4\x3C\xE5\x3C\xE6\x3C\xE7\x3C", b"\xE8\x3C\xE9\x3C\xEA\x3C\xEB\x3C", b"\xEC\x3C\xED\x3C\xEE\x3C\xEF\x3C",
         b"\xED\x80\x00", b"\xED\x80\x7F", b"\xED\x80\xC0", b"\xED\x80\xFF", b"\xED\xA0\x80", b"\xED\xA0\x80\x35",
         b"\xED\xA0\x80\xED\xB0\x80", b"\xED\xA0\x80\xED\xBF\xBF", b"\xED\xA0\x80\xED\xBF\xBF\xED\xAF\x41",
         b"\xED\xAD\xBF", b"\xED\xAD\xBF\xED\xB0\x80", b"\xED\xAD\xBF\xED\xBF\xBF", b"\xED\xAE\x80",
         b"\xED\xAE\x80\xED\xB0\x80", b"\xED\xAE\x80\xED\xBF\xBF", b"\xED\xAF\xBF", b"\xED\xAF\xBF\xED\xB0\x80",
         b"\xED\xAF\xBF\xED\xBF\xBF", b"\xED\xB0\x80", b"\xED\xBE\x80", b"\xED\xBF\xBF", b"\xEF\xBF",
         b"\xF0\x3C\xF1\x3C", b"\xF0\x80\x80", b"\xF0\x80\x80\x80", b"\xF0\x80\x80\xAF", b"\xF0\x80\x81\xBF",
         b"\xF0\x8F\xBF\xBF", b"\xF0\x90\x80\x00", b"\xF0\x90\x80\x7F", b"\xF0\x90\x80\xC0", b"\xF0\x90\x80\xFF",
         b"\xF1\x80\x80\x00", b"\xF1\x80\x80\x7F", b"\xF1\x80\x80\xC0", b"\xF1\x80\x80\xFF", b"\xF2\x3C\xF3\x3C",
         b"\xF4\x3C\xF5\x3C", b"\xF4\x80\x80\x00", b"\xF4\x80\x80\x7F", b"\xF4\x80\x80\xC0", b"\xF4\x80\x80\xFF",
         b"\xF4\x90\x80\x80", b"\xF4\x91\x92\x93\xFF\x41\x80\xBF\x42", b"\xF5\x3C", b"\xF6\x3C\xF7\x3C",
         b"\xF7\xBF\xBF", b"\xF7\xBF\xBF\xBF", b"\xF7\xBF\xBF\xBF\xBF", b"\xF7\xBF\xBF\xBF\xBF\xBF",
         b"\xF7\xBF\xBF\xBF\xBF\xBF\xBF", b"\xF8\x3C", b"\xF8\x80\x80\x80", b"\xF8\x80\x80\x80\xAF",
         b"\xF8\x87\xBF\xBF\xBF", b"\xF8\x88\x80\x80\x80", b"\xF9\x3C", b"\xFA\x3C", b"\xFB\x3C", b"\xFB\xBF\xBF\xBF",
         b"\xFC\x3C", b"\xFC\x80\x80\x80\x80", b"\xFC\x80\x80\x80\x80\xAF", b"\xFC\x84\x80\x80\x80\x80", b"\xFD\x3C",
         b"\xFD\xBF\xBF\xBF\xBF", b"\xFE", b"\xFF", b"\xFF\x3C"
    ];

    for bad in broken_utf8 {
        'next: for len in 1..bad.len() {
            let mut handler = |ch: &[u8]| {
                assert!(
                    !std::str::from_utf8(ch).unwrap().contains('<'),
                    "{ch:x?} of {bad:x?}"
                )
            };
            let mut t = StreamingHandlerSink::new(UTF_8, &mut handler);
            for chunk in bad.chunks(len) {
                if t.write_utf8_chunk(chunk, ContentType::Text).is_err() {
                    continue 'next;
                }
            }
            // An ASCII write forces flush of an incomplete sequence
            assert!(
                t.write_utf8_chunk(b"<", ContentType::Text).is_err(),
                "Shouldn't have allowed {bad:?} {}",
                String::from_utf8_lossy(bad)
            );
        }
    }
}