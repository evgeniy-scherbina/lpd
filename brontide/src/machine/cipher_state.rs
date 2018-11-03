use chacha20_poly1305_aead::DecryptError;
use std::{fmt, io};
use byteorder::{LittleEndian, ByteOrder};

// keyRotationInterval is the number of messages sent on a single
// cipher stream before the keys are rotated forwards.
const KEY_ROTATION_INTERVAL: u64 = 1000;

// MAC_SIZE is the length in bytes of the tags generated by poly1305.
const MAC_SIZE: usize = 16;

/// `CipherState` encapsulates the state for the AEAD which will be used to
/// encrypt+authenticate any payloads sent during the handshake, and messages
/// sent once the handshake has completed.
pub struct CipherState {
    // nonce is the nonce passed into the chacha20-poly1305 instance for
    // encryption+decryption. The nonce is incremented after each successful
    // encryption/decryption.
    //
    // WARNING: this should actually be 96 bit
    nonce: u64,

    // secret_key is the shared symmetric key which will be used to
    // instantiate the cipher.
    //
    // TODO: protect it somehow
    secret_key: [u8; 32],

    // salt is an additional secret which is used during key rotation to
    // generate new keys.
    salt: [u8; 32],
}

impl fmt::Debug for CipherState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            r#"
        nonce:      {:?}
	    secret_key: {:?}
	    salt:       {:?}
        "#,
            self.nonce,
            hex::encode(self.secret_key),
            hex::encode(self.salt),
        )
    }
}

impl CipherState {
    pub fn new(salt: [u8; 32], key: [u8; 32]) -> Self {
        CipherState {
            nonce: 0,
            secret_key: key,
            salt: salt,
        }
    }

    /// `encrypt` returns a `cipher_text` which is the encryption of the `plain_text`
    /// observing the passed `associated_data` within the AEAD construction.
    pub fn encrypt<W: io::Write>(
        &mut self,
        associated_data: &[u8],
        cipher_text: &mut W,
        plain_text: &[u8],
    ) -> Result<[u8; MAC_SIZE], io::Error> {
        use chacha20_poly1305_aead::encrypt;

        let mut nonce: [u8; 12] = [0; 12];
        LittleEndian::write_u64(&mut nonce[4..], self.nonce);
        encrypt(&self.secret_key, &nonce, associated_data, plain_text, cipher_text)
            .map(|t| { self.next(); t })
    }

    /// `decrypt` attempts to decrypt the passed `cipher_text` observing the specified
    /// `associated_data` within the AEAD construction. In the case that the final MAC
    /// check fails, then an error will be returned
    pub fn decrypt<W: io::Write>(
        &mut self,
        associated_data: &[u8],
        plain_text: &mut W,
        cipher_text: &[u8],
        tag: [u8; MAC_SIZE],
    ) -> Result<(), DecryptError> {
        use chacha20_poly1305_aead::decrypt;

        let mut nonce: [u8; 12] = [0; 12];
        LittleEndian::write_u64(&mut nonce[4..], self.nonce);
        decrypt(&self.secret_key, &nonce, associated_data, cipher_text, &tag, plain_text)
            .map(|t| { self.next(); t })
    }

    // ratcheting the current key forward
    // using an HKDF invocation with the salt for the `CipherState` as the salt,
    // and the current key as the input
    fn next(&mut self) {
        use sha2::Sha256;
        use hkdf::Hkdf;

        self.nonce += 1;
        if self.nonce == KEY_ROTATION_INTERVAL {
            let hkdf = Hkdf::<Sha256>::extract(Some(&self.salt), &self.secret_key);
            let okm = hkdf.expand(&[], 64);

            self.salt.copy_from_slice(&okm.as_slice()[..32]);
            self.secret_key.copy_from_slice(&okm.as_slice()[32..]);
            self.nonce = 0;
        }
    }

    #[cfg(test)]
    pub fn secret_key(&self) -> [u8; 32] {
        self.secret_key.clone()
    }
}