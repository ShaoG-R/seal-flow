//! Implements the common logic for synchronous, streaming encryption and decryption.
//! This is the backend for both symmetric and hybrid streaming modes.
//!
//! 实现同步、流式加密和解密的通用逻辑。
//! 这是对称和混合流式模式的后端。

use crate::common::derive_nonce;
use crate::common::header::AeadParams;
use crate::error::{Error, FormatError, Result};
use crate::processor::traits::FinishingWrite;
use seal_crypto_wrapper::prelude::TypedAeadKey;
use seal_crypto_wrapper::traits::AeadAlgorithmTrait;
use seal_crypto_wrapper::wrappers::aead::AeadAlgorithmWrapper;
use std::borrow::Cow;
use std::io::{self, Read, Write};
use std::marker::PhantomData;

// --- Encryptor ---

pub struct StreamingEncryptorSetup<'a> {
    pub aead_params: AeadParams,
    pub(crate) aad: Option<Vec<u8>>,
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> StreamingEncryptorSetup<'a> {
    pub(crate) fn new(aead_params: AeadParams, aad: Option<Vec<u8>>) -> Self {
        Self {
            aead_params,
            aad,
            _lifetime: PhantomData,
        }
    }

    pub fn start<W: Write + 'a>(
        self,
        writer: W,
        key: Cow<'a, TypedAeadKey>,
    ) -> Result<StreamingEncryptor<'a, W>> {
        if self.aead_params.algorithm != key.algorithm() {
            return Err(Error::Format(FormatError::InvalidKeyType.into()));
        }

        let algorithm = AeadAlgorithmWrapper::from_enum(self.aead_params.algorithm);

        let chunk_size = self.aead_params.chunk_size as usize;
        let tag_size = algorithm.tag_size();
        Ok(StreamingEncryptor {
            writer,
            algorithm,
            key: key.into_owned(),
            base_nonce: self.aead_params.base_nonce,
            chunk_size,
            buffer: Vec::with_capacity(chunk_size),
            chunk_counter: 0,
            encrypted_chunk_buffer: vec![0u8; chunk_size + tag_size],
            aad: self.aad,
            _lifetime: PhantomData,
        })
    }
}

pub struct StreamingEncryptor<'a, W: Write> {
    writer: W,
    algorithm: AeadAlgorithmWrapper,
    key: TypedAeadKey,
    base_nonce: Box<[u8]>,
    chunk_size: usize,
    buffer: Vec<u8>,
    chunk_counter: u64,
    encrypted_chunk_buffer: Vec<u8>,
    aad: Option<Vec<u8>>,
    _lifetime: std::marker::PhantomData<&'a ()>,
}

impl<'a, W: Write> FinishingWrite for StreamingEncryptor<'a, W> {
    fn finish(mut self: Box<Self>) -> Result<()> {
        if !self.buffer.is_empty() {
            let nonce = derive_nonce(&self.base_nonce, self.chunk_counter);
            let bytes_written = self.algorithm.encrypt_to_buffer(
                &self.buffer,
                &mut self.encrypted_chunk_buffer,
                &self.key,
                &nonce,
                self.aad.as_deref(),
            )?;
            self.writer
                .write_all(&self.encrypted_chunk_buffer[..bytes_written])?;
            self.chunk_counter += 1;
            self.buffer.clear();
        }
        self.writer.flush()?;
        Ok(())
    }
}

impl<'a, W: Write> Write for StreamingEncryptor<'a, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut input = buf;

        if !self.buffer.is_empty() {
            let space_in_buffer = self.chunk_size - self.buffer.len();
            let fill_len = std::cmp::min(space_in_buffer, input.len());
            self.buffer.extend_from_slice(&input[..fill_len]);
            input = &input[fill_len..];

            if self.buffer.len() == self.chunk_size {
                let nonce = derive_nonce(&self.base_nonce, self.chunk_counter);

                let bytes_written = self
                    .algorithm
                    .encrypt_to_buffer(
                        &self.buffer,
                        &mut self.encrypted_chunk_buffer,
                        &self.key,
                        &nonce,
                        self.aad.as_deref(),
                    )
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                self.writer
                    .write_all(&self.encrypted_chunk_buffer[..bytes_written])?;
                self.chunk_counter += 1;
                self.buffer.clear();
            }
        }

        while input.len() >= self.chunk_size {
            let chunk = &input[..self.chunk_size];
            let nonce = derive_nonce(&self.base_nonce, self.chunk_counter);

            let bytes_written = self
                .algorithm
                .encrypt_to_buffer(
                    chunk,
                    &mut self.encrypted_chunk_buffer,
                    &self.key,
                    &nonce,
                    self.aad.as_deref(),
                )
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            self.writer
                .write_all(&self.encrypted_chunk_buffer[..bytes_written])?;

            self.chunk_counter += 1;
            input = &input[self.chunk_size..];
        }

        if !input.is_empty() {
            self.buffer.extend_from_slice(input);
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

// --- Decryptor ---

pub struct StreamingDecryptorSetup<'a> {
    pub(crate) algorithm: AeadAlgorithmWrapper,
    pub(crate) nonce: Box<[u8]>,
    pub(crate) chunk_size: usize,
    pub(crate) aad: Option<Vec<u8>>,
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> StreamingDecryptorSetup<'a> {
    pub(crate) fn new(
        algorithm: AeadAlgorithmWrapper,
        nonce: Box<[u8]>,
        chunk_size: usize,
        aad: Option<Vec<u8>>,
    ) -> Self {
        Self {
            algorithm,
            nonce,
            chunk_size,
            aad,
            _lifetime: PhantomData,
        }
    }

    pub fn start<R: Read + 'a>(
        self,
        reader: R,
        key: Cow<'a, TypedAeadKey>,
    ) -> StreamingDecryptor<'a, R> {
        let encrypted_chunk_size = self.chunk_size + self.algorithm.tag_size();
        let algorithm = self.algorithm.clone();
        StreamingDecryptor {
            reader,
            algorithm,
            key: key.into_owned(),
            base_nonce: self.nonce,
            encrypted_chunk_size,
            buffer: io::Cursor::new(Vec::new()),
            encrypted_chunk_buffer: vec![0; encrypted_chunk_size],
            chunk_counter: 0,
            is_done: false,
            aad: self.aad,
            _lifetime: PhantomData,
        }
    }
}

pub struct StreamingDecryptor<'a, R: Read> {
    reader: R,
    algorithm: AeadAlgorithmWrapper,
    key: TypedAeadKey,
    base_nonce: Box<[u8]>,
    encrypted_chunk_size: usize,
    buffer: io::Cursor<Vec<u8>>,
    encrypted_chunk_buffer: Vec<u8>,
    chunk_counter: u64,
    is_done: bool,
    aad: Option<Vec<u8>>,
    _lifetime: std::marker::PhantomData<&'a ()>,
}

impl<'a, R: Read> Read for StreamingDecryptor<'a, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let bytes_read_from_buf = self.buffer.read(buf)?;
        if bytes_read_from_buf > 0 {
            return Ok(bytes_read_from_buf);
        }

        if self.is_done {
            return Ok(0);
        }

        let mut total_bytes_read = 0;
        while total_bytes_read < self.encrypted_chunk_size {
            match self
                .reader
                .read(&mut self.encrypted_chunk_buffer[total_bytes_read..])
            {
                Ok(0) => {
                    self.is_done = true;
                    break;
                }
                Ok(n) => total_bytes_read += n,
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }

        if total_bytes_read == 0 {
            return Ok(0);
        }

        let nonce = derive_nonce(&self.base_nonce, self.chunk_counter);

        let decrypted_buf = self.buffer.get_mut();
        decrypted_buf.clear();
        decrypted_buf.resize(self.encrypted_chunk_size, 0);

        let bytes_written = self
            .algorithm
            .decrypt_to_buffer(
                &self.encrypted_chunk_buffer[..total_bytes_read],
                decrypted_buf,
                &self.key,
                &nonce,
                self.aad.as_deref(),
            )
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        decrypted_buf.truncate(bytes_written);
        self.buffer.set_position(0);
        self.chunk_counter += 1;

        self.buffer.read(buf)
    }
}
