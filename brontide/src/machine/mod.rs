#[cfg(test)]
mod test_bolt0008;

use tokio_core::io::read;
use std::{fmt, io, error};
use secp256k1::{PublicKey, SecretKey, Error};
use sha2::{Sha256, Digest};
use byteorder::{ByteOrder, LittleEndian, BigEndian};

use hex;
use hkdf;
use std;
use rand;
use chacha20_poly1305_aead;

#[derive(Debug)]
pub enum HandshakeError {
    Io(io::Error),
    Crypto(Error),
    UnknownHandshakeVersion(String),
    NotInitializedYet,
}

impl error::Error for HandshakeError {
    fn cause(&self) -> Option<&dyn error::Error> {
        use self::HandshakeError::*;

        match self {
            &Io(ref e) => Some(e),
            &Crypto(ref e) => Some(e),
            _ => None,
        }
    }
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::HandshakeError::*;

        match self {
            &Io(ref e) => write!(f, "io error: {}", e),
            &Crypto(ref e) => write!(f, "crypto error: {}", e),
            &UnknownHandshakeVersion(ref msg) => write!(f, "{}", msg),
            &NotInitializedYet => write!(f, "not initialized yet")
        }
    }
}

// PROTOCOL_NAME is the precise instantiation of the Noise protocol
// handshake at the center of Brontide. This value will be used as part
// of the prologue. If the initiator and responder aren't using the
// exact same string for this value, along with prologue of the Bitcoin
// network, then the initial handshake will fail.
static PROTOCOL_NAME: &'static str = "Noise_XK_secp256k1_ChaChaPoly_SHA256";

// MAC_SIZE is the length in bytes of the tags generated by poly1305.
const MAC_SIZE: usize = 16;

// LENGTH_HEADER_SIZE is the number of bytes used to prefix encode the
// length of a message payload.
const LENGTH_HEADER_SIZE: usize = 2;

// keyRotationInterval is the number of messages sent on a single
// cipher stream before the keys are rotated forwards.
const KEY_ROTATION_INTERVAL: u16 = 1000;

// HANDSHAKE_READ_TIMEOUT is a read timeout that will be enforced when
// waiting for data payloads during the various acts of Brontide. If
// the remote party fails to deliver the proper payload within this
// time frame, then we'll fail the connection.
static _HANDSHAKE_READ_TIMEOUT: u8 = 5;

// ERR_MAX_MESSAGE_LENGTH_EXCEEDED is returned a message to be written to
// the cipher session exceeds the maximum allowed message payload.
static ERR_MAX_MESSAGE_LENGTH_EXCEEDED: &'static str = "the generated payload exceeds the max allowed message length of (2^16)-1";

// ecdh performs an ECDH operation between public and private. The returned value is
// the sha256 of the compressed shared point.
fn ecdh(pk: &PublicKey, sk: &SecretKey) -> Result<[u8; 32], Error> {
    use secp256k1::Secp256k1;

    let mut pk_cloned = pk.clone();
    pk_cloned.mul_assign(&Secp256k1::new(), sk)?;

    let mut hasher = Sha256::default();
    hasher.input(&pk_cloned.serialize());
    let hash = hasher.result();

    let mut array: [u8; 32] = [0; 32];
    array.copy_from_slice(&hash);
    Ok(array)
}

// TODO(evg): we have changed encrypt/decrypt and encrypt_and_hash/decrypt_and_hash method signatures
// so it should be reflect in doc

// CipherState encapsulates the state for the AEAD which will be used to
// encrypt+authenticate any payloads sent during the handshake, and messages
// sent once the handshake has completed.
struct CipherState {
    // nonce is the nonce passed into the chacha20-poly1305 instance for
    // encryption+decryption. The nonce is incremented after each successful
    // encryption/decryption.
    //
    // TODO(roasbeef): this should actually be 96 bit
    nonce: u64,

    // secret_key is the shared symmetric key which will be used to
    // instantiate the cipher.
    //
    // TODO(roasbeef): m-lock??
    secret_key: [u8; 32],

    // salt is an additional secret which is used during key rotation to
    // generate new keys.
    salt: [u8; 32],

    // cipher is an instance of the ChaCha20-Poly1305 AEAD construction
    // created using the secretKey above.
//	cipher cipher.AEAD
}

impl fmt::Debug for CipherState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, r#"
        nonce:      {:?}
	    secret_key: {:?}
	    salt:       {:?}
        "#, self.nonce, hex::encode(self.secret_key), hex::encode(self.salt),
        )
    }
}

impl CipherState {
    // TODO(evg): implement Default instead?
    fn empty() -> Self {
        Self {
            nonce: 0,
            secret_key: [0; 32],
            salt: [0; 32],
        }
    }

    // encrypt returns a ciphertext which is the encryption of the plainText
    // observing the passed associatedData within the AEAD construction.
    fn encrypt(&mut self, associated_data: &[u8], cipher_text: &mut Vec<u8>, plain_text: &[u8]) -> Result<[u8; MAC_SIZE], io::Error> {
        let mut nonce: [u8; 12] = [0; 12];
        LittleEndian::write_u64(&mut nonce[4..], self.nonce);
        let tag = chacha20_poly1305_aead::encrypt(
            &self.secret_key, &nonce, associated_data, plain_text, cipher_text)?;

        self.nonce += 1;
        if self.nonce == KEY_ROTATION_INTERVAL as u64 {
            self.rotate_key();
        }
        Ok(tag)
    }

    // decrypt attempts to decrypt the passed ciphertext observing the specified
    // associatedData within the AEAD construction. In the case that the final MAC
    // check fails, then a non-nil error will be returned.
    fn decrypt<W: io::Write>(&mut self, associated_data: &[u8], plain_text: &mut W, cipher_text: &[u8], tag: [u8; MAC_SIZE]) -> Result<(), io::Error> {
        let mut nonce: [u8; 12] = [0; 12];
        LittleEndian::write_u64(&mut nonce[4..], self.nonce);
        chacha20_poly1305_aead::decrypt(
            &self.secret_key, &nonce, associated_data, cipher_text, &tag, plain_text)?;

        self.nonce += 1;
        if self.nonce == KEY_ROTATION_INTERVAL as u64 {
            self.rotate_key();
        }
        Ok(())
    }

    // initialize_key initializes the secret key and AEAD cipher scheme based off of
    // the passed key.
    fn initialize_key(&mut self, key: [u8; 32]) {
        self.secret_key = key;
        self.nonce = 0;

        // Safe to ignore the error here as our key is properly sized
        // (32-bytes).
        // c.cipher, _ = chacha20poly1305.New(c.secretKey[:])
    }

    // initialize_key_with_salt is identical to InitializeKey however it also sets the
    // cipherState's salt field which is used for key rotation.
    fn initialize_key_with_salt(&mut self, salt: [u8; 32], key: [u8; 32]) {
        self.salt = salt;
        self.initialize_key(key);
    }

    // rotate_key rotates the current encryption/decryption key for this cipherState
    // instance. Key rotation is performed by ratcheting the current key forward
    // using an HKDF invocation with the cipherState's salt as the salt, and the
    // current key as the input.
    fn rotate_key(&mut self) {
        let hkdf = hkdf::Hkdf::<Sha256>::extract(Some(&self.salt), &self.secret_key);
        let info: &[u8] = &[];
        let okm = hkdf.expand(info, 64);

        self.salt.copy_from_slice(&okm.as_slice()[..32]);
        let mut next_key: [u8; 32] = [0; 32];
        next_key.copy_from_slice(&okm.as_slice()[32..]);

        self.initialize_key(next_key);
    }
}

// SymmetricState encapsulates a cipherState object and houses the ephemeral
// handshake digest state. This struct is used during the handshake to derive
// new shared secrets based off of the result of ECDH operations. Ultimately,
// the final key yielded by this struct is the result of an incremental
// Triple-DH operation.
struct SymmetricState {
    cipher_state: CipherState,

    // chaining_key is used as the salt to the HKDF function to derive a new
    // chaining key as well as a new tempKey which is used for
    // encryption/decryption.
    chaining_key: [u8; 32],

    // temp_key is the latter 32 bytes resulted from the latest HKDF
    // iteration. This key is used to encrypt/decrypt any handshake
    // messages or payloads sent until the next DH operation is executed.
    temp_key: [u8; 32],

    // handshake_digest is the cumulative hash digest of all handshake
    // messages sent from start to finish. This value is never transmitted
    // to the other side, but will be used as the AD when
    // encrypting/decrypting messages using our AEAD construction.
    handshake_digest: [u8; 32],
}

impl fmt::Debug for SymmetricState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, r#"
        cipher_state:     {:?}
        chaining_key:     {:?}
        temp_key:         {:?}
        handshake_digest: {:?}
        "#, self.cipher_state, hex::encode(self.chaining_key),
               hex::encode(self.temp_key), hex::encode(self.handshake_digest),
        )
    }
}

impl SymmetricState {
    fn empty() -> Self {
        Self {
            cipher_state: CipherState::empty(),
            chaining_key: [0; 32],
            temp_key: [0; 32],
            handshake_digest: [0; 32],
        }
    }

    // mix_key is implements a basic HKDF-based key ratchet. This method is called
    // with the result of each DH output generated during the handshake process.
    // The first 32 bytes extract from the HKDF reader is the next chaining key,
    // then latter 32 bytes become the temp secret key using within any future AEAD
    // operations until another DH operation is performed.
    fn mix_key(&mut self, input: &[u8]) {
        let mut salt: [u8; 32] = [0; 32];
        salt.copy_from_slice(&self.chaining_key);
        let hkdf = hkdf::Hkdf::<Sha256>::extract(Some(&salt), input);

        let info: &[u8] = &[];
        let okm = hkdf.expand(info, 64);

        self.chaining_key.copy_from_slice(&okm.as_slice()[..32]);
        self.temp_key.copy_from_slice(&okm.as_slice()[32..]);

        self.cipher_state.initialize_key(self.temp_key);
    }

    // mix_hash hashes the passed input data into the cumulative handshake digest.
    // The running result of this value (h) is used as the associated data in all
    // decryption/encryption operations.
    fn mix_hash(&mut self, data: &[u8]) {
        let mut hasher = Sha256::default();
        hasher.input(&self.handshake_digest);
        hasher.input(data);

        self.handshake_digest.copy_from_slice(&hasher.result()[..]);
    }

    // encrypt_and_hash returns the authenticated encryption of the passed plaintext.
    // When encrypting the handshake digest (h) is used as the associated data to
    // the AEAD cipher
    fn encrypt_and_hash(&mut self, plaintext: &[u8], cipher_text: &mut Vec<u8>) -> Result<[u8; MAC_SIZE], io::Error> {
        let tag = self.cipher_state.encrypt(
            &self.handshake_digest, cipher_text, plaintext)?;

        // To be compliant with golang's implementation of chacha20poly1305 and brontide packages
        // we concatenate cipher_text and mac for mixing with internal state.
        let mut cipher_text_with_mac: Vec<u8> = Vec::new();
        for item in cipher_text.clone() {
            cipher_text_with_mac.push(item.clone());
        }
        for item in &tag {
            cipher_text_with_mac.push(item.clone());
        }

        self.mix_hash(&mut cipher_text_with_mac);

        Ok(tag)
    }

    // decrypt_and_hash returns the authenticated decryption of the passed
    // ciphertext.  When encrypting the handshake digest (h) is used as the
    // associated data to the AEAD cipher.
    fn decrypt_and_hash(&mut self, ciphertext: &[u8], tag: [u8; MAC_SIZE]) -> Result<Vec<u8>, io::Error> {
        let mut plaintext: Vec<u8> = Vec::new();
        self.cipher_state.decrypt(&self.handshake_digest, &mut plaintext, ciphertext, tag)?;

        let mut cipher_text_with_mac: Vec<u8> = Vec::new();
        for item in ciphertext.clone() {
            cipher_text_with_mac.push(item.clone());
        }
        for item in &tag {
            cipher_text_with_mac.push(item.clone());
        }

        self.mix_hash(&cipher_text_with_mac);

        Ok(plaintext)
    }

    // initialize_symmetric initializes the symmetric state by setting the handshake
    // digest (h) and the chaining key (ck) to protocol name.
    fn initialize_symmetric(&mut self, protocol_name: &[u8]) {
        let empty: [u8; 32] = [0; 32];

        let mut hasher = Sha256::default();
        hasher.input(protocol_name);
        self.handshake_digest.copy_from_slice(&hasher.result()[..]);
        self.chaining_key = self.handshake_digest;
        self.cipher_state.initialize_key(empty);
    }
}

// HandshakeState encapsulates the symmetricState and keeps track of all the
// public keys (static and ephemeral) for both sides during the handshake
// transcript. If the handshake completes successfully, then two instances of a
// cipherState are emitted: one to encrypt messages from initiator to
// responder, and the other for the opposite direction.
struct HandshakeState {
    symmetric_state: SymmetricState,

    initiator: bool,

    local_static:    SecretKey,
    // if None means not initialized
    local_ephemeral: Option<SecretKey>,

    remote_static:    PublicKey,
    // if None means not initialized
    remote_ephemeral: Option<PublicKey>,
}

impl fmt::Debug for HandshakeState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut remote_ephemeral_str = String::from("None");
        if self.remote_ephemeral.is_some() {
            remote_ephemeral_str = hex::encode(&self.remote_ephemeral.unwrap().serialize()[..]);
        }

        write!(f, r#"
        symmetric_state: {:?}

        initiator: {:?}

        local_static:    {:?}
        local_ephemeral: {:?}

        remote_static:    {:?}
        remote_ephemeral: {:?}
        "#, self.symmetric_state, self.initiator,
               self.local_static, self.local_ephemeral,
               hex::encode(&self.remote_static.serialize()[..]), remote_ephemeral_str,
        )
    }
}

impl HandshakeState {
    // new returns a new instance of the handshake state initialized
    // with the prologue and protocol name. If this is the responder's handshake
    // state, then the remotePub can be nil.
    fn new(initiator: bool, prologue: &[u8],
           local_priv: SecretKey, remote_pub: PublicKey) -> Result<Self, Error> {
        use secp256k1::Secp256k1;

        let mut h = HandshakeState{
            symmetric_state: SymmetricState::empty(),
            initiator,
            local_static:     local_priv,
            local_ephemeral:  None,
            remote_static:    remote_pub,
            remote_ephemeral: None,
        };

        // Set the current chaining key and handshake digest to the hash of the
        // protocol name, and additionally mix in the prologue. If either sides
        // disagree about the prologue or protocol name, then the handshake
        // will fail.
        h.symmetric_state.initialize_symmetric(PROTOCOL_NAME.as_bytes());
        h.symmetric_state.mix_hash(prologue);

        // In Noise_XK, then initiator should know the responder's static
        // public key, therefore we include the responder's static key in the
        // handshake digest. If the initiator gets this value wrong, then the
        // handshake will fail.
        if initiator {
            h.symmetric_state.mix_hash(&remote_pub.serialize())
        } else {
            let local_pub = PublicKey::from_secret_key(&Secp256k1::new(), &local_priv)?;
            h.symmetric_state.mix_hash(&local_pub.serialize())
        }

        Ok(h)
    }
}

pub struct Machine {
    send_cipher: CipherState,
    recv_cipher: CipherState,

    initiator: bool,

    ephemeral_gen: fn() -> Result<SecretKey, Error>,

    handshake_state: HandshakeState,

    // next_cipher_header is a static buffer that we'll use to read in the
    // next ciphertext header from the wire. The header is a 2 byte length
    // (of the next ciphertext), followed by a 16 byte MAC.
    next_cipher_header: [u8; LENGTH_HEADER_SIZE + MAC_SIZE],

    // next_cipher_text is a static buffer that we'll use to read in the
    // bytes of the next cipher text message. As all messages in the
    // protocol MUST be below 65KB plus our macSize, this will be
    // sufficient to buffer all messages from the socket when we need to
    // read the next one. Having a fixed buffer that's re-used also means
    // that we save on allocations as we don't need to create a new one
    // each time.
    next_cipher_text: [u8; std::u16::MAX as usize + MAC_SIZE],
}

impl fmt::Debug for Machine {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, r#"
        send_cipher:     {:?}
        recv_cipher:     {:?}
        handshake_state: {:?}
        "#, self.send_cipher, self.recv_cipher, self.handshake_state,
        )
    }
}

impl Machine {
    // new creates a new instance of the brontide state-machine. If
    // the responder (listener) is creating the object, then the remotePub should
    // be nil. The handshake state within brontide is initialized using the ascii
    // string "lightning" as the prologue. The last parameter is a set of variadic
    // arguments for adding additional options to the brontide Machine
    // initialization.
    pub fn new<F>(initiator: bool, local_priv: SecretKey, remote_pub: PublicKey, options: &[F]) -> Result<Self, Error> where F: Fn(&mut Self) {
        use secp256k1::{Secp256k1, constants::SECRET_KEY_SIZE};

        let handshake = HandshakeState::new(initiator, "lightning".as_bytes(), local_priv, remote_pub)?;

        let mut m = Machine {
            send_cipher: CipherState::empty(),
            recv_cipher: CipherState::empty(),
            initiator: initiator,
            // With the initial base machine created, we'll assign our default
            // version of the ephemeral key generator.
            ephemeral_gen: || {
                let sk_bytes: [u8; SECRET_KEY_SIZE] = rand::random();
                let sk = SecretKey::from_slice(&Secp256k1::new(), &sk_bytes)?;
                Ok(sk)
            },
            handshake_state: handshake,
            next_cipher_header: [0; LENGTH_HEADER_SIZE + MAC_SIZE],
            next_cipher_text:   [0; std::u16::MAX as usize + MAC_SIZE],
        };

        // With the default options established, we'll now process all the
        // options passed in as parameters.
        for option in options {
            option(&mut m)
        }

        Ok(m)
    }

    pub fn handshake<T>(&mut self, remote: &mut T) -> Result<PublicKey, HandshakeError>
    where
        T: io::Read + io::Write,
    {
        if self.initiator {
            self.gen_act_one()?.write(remote).map_err(HandshakeError::Io)?;
            self.recv_act_two(ActTwo::read(remote).map_err(HandshakeError::Io)?)?;
            self.gen_act_three()?.write(remote).map_err(HandshakeError::Io)?;
        } else {
            self.recv_act_one(ActOne::read(remote).map_err(HandshakeError::Io)?)?;
            self.gen_act_two()?.write(remote).map_err(HandshakeError::Io)?;
            self.recv_act_three(ActThree::read(remote).map_err(HandshakeError::Io)?)?;
        }

        Ok(self.handshake_state.remote_static.clone())
    }
}

// HANDSHAKE_VERSION is the expected version of the brontide handshake.
// Any messages that carry a different version will cause the handshake
// to abort immediately.
#[repr(u8)]
#[derive(Eq, PartialEq)]
enum HandshakeVersion {
    _0 = 0,
}

// ACT_ONE_SIZE is the size of the packet sent from initiator to
// responder in ActOne. The packet consists of a handshake version, an
// ephemeral key in compressed format, and a 16-byte poly1305 tag.
//
// 1 + 33 + 16
struct ActOne {
    bytes: [u8; 1 + 33 + MAC_SIZE],
}

impl ActOne {
    const SIZE: usize = 1 + 33 + MAC_SIZE;

    fn read<R>(source: &mut R) -> Result<Self, io::Error> where R: io::Read {
        let mut bytes = [0; Self::SIZE];
        source.read_exact(&mut bytes)?;
        Ok(ActOne { bytes: bytes })
    }

    fn write<W>(self, destination: &mut W) -> Result<(), io::Error> where W: io::Write {
        destination.write_all(&self.bytes[..])
    }

    fn new(version: HandshakeVersion, key: [u8; 33], tag: [u8; MAC_SIZE]) -> Self {
        let mut s = ActOne {
            bytes: [0; Self::SIZE],
        };
        s.bytes[0] = version as _;
        s.bytes[1..34].copy_from_slice(&key);
        s.bytes[34..].copy_from_slice(&tag);
        s
    }

    fn version(&self) -> Result<HandshakeVersion, ()> {
        match self.bytes[0] {
            0 => Ok(HandshakeVersion::_0),
            _ => Err(())
        }
    }

    fn key(&self) -> Result<PublicKey, Error> {
        use secp256k1::Secp256k1;

        PublicKey::from_slice(&Secp256k1::new(), &self.bytes[1..34])
    }

    fn tag(&self) -> [u8; MAC_SIZE] {
        let mut v = [0; MAC_SIZE];
        v.copy_from_slice(&self.bytes[34..]);
        v
    }
}

// ACT_TWO_SIZE is the size the packet sent from responder to initiator
// in ActTwo. The packet consists of a handshake version, an ephemeral
// key in compressed format and a 16-byte poly1305 tag.
//
// 1 + 33 + 16
type ActTwo = ActOne;

// ACT_THREE_SIZE is the size of the packet sent from initiator to
// responder in ActThree. The packet consists of a handshake version,
// the initiators static key encrypted with strong forward secrecy and
// a 16-byte poly1035
// tag.
//
// 1 + 33 + 16 + 16
struct ActThree {
    bytes: [u8; 1 + 33 + 16 + 16],
}

impl ActThree {
    const SIZE: usize = 1 + 33 + 2 * MAC_SIZE;

    fn read<R>(mut source: R) -> Result<Self, io::Error> where R: io::Read {
        let mut bytes = [0; Self::SIZE];
        source.read_exact(&mut bytes)?;
        Ok(ActThree { bytes: bytes })
    }

    fn write<W>(self, mut destination: W) -> Result<(), io::Error> where W: io::Write {
        destination.write_all(&self.bytes[..])
    }

    fn new(version: HandshakeVersion, key: Vec<u8>, tag_first: [u8; MAC_SIZE], tag_second: [u8; MAC_SIZE]) -> Self {
        let mut s = ActThree {
            bytes: [0; Self::SIZE],
        };
        s.bytes[0] = version as _;
        s.bytes[1..34].copy_from_slice(&key);
        s.bytes[34..50].copy_from_slice(&tag_first);
        s.bytes[50..].copy_from_slice(&tag_second);
        s
    }

    fn version(&self) -> Result<HandshakeVersion, ()> {
        match self.bytes[0] {
            0 => Ok(HandshakeVersion::_0),
            _ => Err(())
        }
    }

    fn key(&self) -> &[u8] {
        &self.bytes[1..34]
    }

    fn tag_first(&self) -> [u8; MAC_SIZE] {
        let mut v = [0; MAC_SIZE];
        v.copy_from_slice(&self.bytes[34..50]);
        v
    }

    fn tag_second(&self) -> [u8; MAC_SIZE] {
        let mut v = [0; MAC_SIZE];
        v.copy_from_slice(&self.bytes[50..]);
        v
    }
}

impl Machine {
    // gen_act_one generates the initial packet (act one) to be sent from initiator
    // to responder. During act one the initiator generates a fresh ephemeral key,
    // hashes it into the handshake digest, and performs an ECDH between this key
    // and the responder's static key. Future payloads are encrypted with a key
    // derived from this result.
    //
    //    -> e, es
    fn gen_act_one(&mut self) -> Result<ActOne, HandshakeError> {
        use secp256k1::Secp256k1;

        // e
        let local_ephemeral_priv = (self.ephemeral_gen)()
            .map_err(HandshakeError::Crypto)?;
        self.handshake_state.local_ephemeral = Some(local_ephemeral_priv);

        let local_ephemeral_pub = PublicKey::from_secret_key(&Secp256k1::new(), &local_ephemeral_priv)
            .map_err(HandshakeError::Crypto)?;
        let ephemeral = local_ephemeral_pub.serialize();
        self.handshake_state.symmetric_state.mix_hash(&ephemeral);

        // es
        let s = ecdh(&self.handshake_state.remote_static, &local_ephemeral_priv)
            .map_err(HandshakeError::Crypto)?;
        self.handshake_state.symmetric_state.mix_key(&s);

        let auth_payload = self.handshake_state.symmetric_state
            .encrypt_and_hash(&[], &mut Vec::new())
            .map_err(HandshakeError::Io)?;

        Ok(ActOne::new(HandshakeVersion::_0, ephemeral, auth_payload))
    }

    // recv_act_one processes the act one packet sent by the initiator. The responder
    // executes the mirrored actions to that of the initiator extending the
    // handshake digest and deriving a new shared secret based on an ECDH with the
    // initiator's ephemeral key and responder's static key.
    fn recv_act_one(&mut self, act_one: ActOne) -> Result<(), HandshakeError> {
        // If the handshake version is unknown, then the handshake fails
        // immediately.
        if let Err(()) = act_one.version() {
            let msg = format!("Act One: invalid handshake version: {}", act_one.bytes[0]);
            return Err(HandshakeError::UnknownHandshakeVersion(msg))
        }

        // e
        let remote_ephemeral = act_one.key()
            .map_err(HandshakeError::Crypto)?;
        self.handshake_state.remote_ephemeral = Some(remote_ephemeral);
        self.handshake_state.symmetric_state.mix_hash(&remote_ephemeral.serialize());

        // es
        let s = ecdh(&remote_ephemeral, &self.handshake_state.local_static)
            .map_err(HandshakeError::Crypto)?;
        self.handshake_state.symmetric_state.mix_key(&s);

        // If the initiator doesn't know our static key, then this operation
        // will fail.
        self.handshake_state.symmetric_state
            .decrypt_and_hash(&[], act_one.tag())
            .map_err(HandshakeError::Io)?;

        Ok(())
    }

    // gen_act_two generates the second packet (act two) to be sent from the
    // responder to the initiator. The packet for act two is identify to that of
    // act one, but then results in a different ECDH operation between the
    // initiator's and responder's ephemeral keys.
    //
    //    <- e, ee
    fn gen_act_two(&mut self) -> Result<ActTwo, HandshakeError> {
        use secp256k1::Secp256k1;

        // e
        let local_ephemeral_priv = (self.ephemeral_gen)().map_err(HandshakeError::Crypto)?;
        self.handshake_state.local_ephemeral = Some(local_ephemeral_priv);

        let local_ephemeral_pub = PublicKey::from_secret_key(
            &Secp256k1::new(), &local_ephemeral_priv).map_err(HandshakeError::Crypto)?;
        let ephemeral = local_ephemeral_pub.serialize();
        self.handshake_state.symmetric_state.mix_hash(&ephemeral);

        // ee
        let s = ecdh(&self.handshake_state.remote_ephemeral.ok_or(HandshakeError::NotInitializedYet)?, &local_ephemeral_priv)
            .map_err(HandshakeError::Crypto)?;
        self.handshake_state.symmetric_state.mix_key(&s);

        let auth_payload = self.handshake_state.symmetric_state
            .encrypt_and_hash(&[], &mut Vec::new())
            .map_err(HandshakeError::Io)?;

        Ok(ActTwo::new(HandshakeVersion::_0, ephemeral, auth_payload))
    }

    // recv_act_two processes the second packet (act two) sent from the responder to
    // the initiator. A successful processing of this packet authenticates the
    // initiator to the responder.
    fn recv_act_two(&mut self, act_two: ActTwo) -> Result<(), HandshakeError> {
        // If the handshake version is unknown, then the handshake fails
        // immediately.
        if let Err(()) = act_two.version() {
            let msg = format!("Act Two: invalid handshake version: {}", act_two.bytes[0]);
            return Err(HandshakeError::UnknownHandshakeVersion(msg))
        }

        // e
        let remote_ephemeral = act_two.key()
            .map_err(HandshakeError::Crypto)?;
        self.handshake_state.remote_ephemeral = Some(remote_ephemeral);
        self.handshake_state.symmetric_state.mix_hash(&remote_ephemeral.serialize());

        // ee
        let s = ecdh(&remote_ephemeral, &self.handshake_state.local_ephemeral.ok_or(HandshakeError::NotInitializedYet)?)
            .map_err(HandshakeError::Crypto)?;
        self.handshake_state.symmetric_state.mix_key(&s);

        self.handshake_state.symmetric_state
            .decrypt_and_hash(&mut Vec::new(), act_two.tag())
            .map_err(HandshakeError::Io)?;
        Ok(())
    }

    // gen_act_three creates the final (act three) packet of the handshake. Act three
    // is to be sent from the initiator to the responder. The purpose of act three
    // is to transmit the initiator's public key under strong forward secrecy to
    // the responder. This act also includes the final ECDH operation which yields
    // the final session.
    //
    //    -> s, se
    fn gen_act_three(&mut self) -> Result<ActThree, HandshakeError> {
        use secp256k1::{Secp256k1, constants::PUBLIC_KEY_SIZE};

        let local_static_pub = PublicKey::from_secret_key(&Secp256k1::new(), &self.handshake_state.local_static)
            .map_err(HandshakeError::Crypto)?;
        let our_pubkey = local_static_pub.serialize();
        let mut ciphertext = Vec::with_capacity(PUBLIC_KEY_SIZE);
        let tag = self.handshake_state.symmetric_state
            .encrypt_and_hash(&our_pubkey, &mut ciphertext)
            .map_err(HandshakeError::Io)?;

        let s = ecdh(&self.handshake_state.remote_ephemeral.ok_or(HandshakeError::NotInitializedYet)?, &self.handshake_state.local_static)
            .map_err(HandshakeError::Crypto)?;
        self.handshake_state.symmetric_state.mix_key(&s);

        let auth_payload = self.handshake_state.symmetric_state
            .encrypt_and_hash(&[], &mut Vec::new())
            .map_err(HandshakeError::Io)?;

        let act_three = ActThree::new(HandshakeVersion::_0, ciphertext, tag, auth_payload);

        // With the final ECDH operation complete, derive the session sending
        // and receiving keys.
        self.split();

        Ok(act_three)
    }

    // recv_act_three processes the final act (act three) sent from the initiator to
    // the responder. After processing this act, the responder learns of the
    // initiator's static public key. Decryption of the static key serves to
    // authenticate the initiator to the responder.
    fn recv_act_three(&mut self, act_three: ActThree) -> Result<(), HandshakeError> {
        use secp256k1::Secp256k1;

        // If the handshake version is unknown, then the handshake fails
        // immediately.
        if let Err(()) = act_three.version() {
            let msg = format!("Act Three: invalid handshake version: {}", act_three.bytes[0]);
            return Err(HandshakeError::UnknownHandshakeVersion(msg))
        }

        // s
        let remote_pub = self.handshake_state.symmetric_state.decrypt_and_hash(act_three.key(), act_three.tag_first())
            .map_err(HandshakeError::Io)?;
        self.handshake_state.remote_static = PublicKey::from_slice(&Secp256k1::new(), &remote_pub)
            .map_err(HandshakeError::Crypto)?;

        // se
        let se = ecdh(&self.handshake_state.remote_static, &self.handshake_state.local_ephemeral.ok_or(HandshakeError::NotInitializedYet)?)
            .map_err(HandshakeError::Crypto)?;
        self.handshake_state.symmetric_state.mix_key(&se);

        self.handshake_state.symmetric_state
            .decrypt_and_hash(&[], act_three.tag_second())
            .map_err(HandshakeError::Io)?;

        // With the final ECDH operation complete, derive the session sending
        // and receiving keys.
        self.split();

        Ok(())
    }

    // split is the final wrap-up act to be executed at the end of a successful
    // three act handshake. This function creates two internal cipherState
    // instances: one which is used to encrypt messages from the initiator to the
    // responder, and another which is used to encrypt message for the opposite
    // direction.
    fn split(&mut self) {
        let mut send_key: [u8; 32] = [0; 32];
        let mut recv_key: [u8; 32] = [0; 32];

        let hkdf = hkdf::Hkdf::<Sha256>::extract(Some(&self.handshake_state.symmetric_state.chaining_key), &[]);
        let okm = hkdf.expand(&[], 64);

        // If we're the initiator the first 32 bytes are used to encrypt our
        // messages and the second 32-bytes to decrypt their messages. For the
        // responder the opposite is true.
        if self.handshake_state.initiator {
            send_key.copy_from_slice(&okm.as_slice()[..32]);
            self.send_cipher.initialize_key_with_salt(self.handshake_state.symmetric_state.chaining_key, send_key);

            recv_key.copy_from_slice(&okm.as_slice()[32..]);
            self.recv_cipher.initialize_key_with_salt(self.handshake_state.symmetric_state.chaining_key, recv_key);
        } else {
            recv_key.copy_from_slice(&okm.as_slice()[..32]);
            self.recv_cipher.initialize_key_with_salt(self.handshake_state.symmetric_state.chaining_key, recv_key);

            send_key.copy_from_slice(&okm.as_slice()[32..]);
            self.send_cipher.initialize_key_with_salt(self.handshake_state.symmetric_state.chaining_key, send_key);
        }
    }

    // write_message writes the next message p to the passed io.Writer. The
    // ciphertext of the message is prepended with an encrypt+auth'd length which
    // must be used as the AD to the AEAD construction when being decrypted by the
    // other side.
    pub fn write_message<W: io::Write>(&mut self, w: &mut W, p: &[u8]) -> Result<(), io::Error> {
        // The total length of each message payload including the MAC size
        // payload exceed the largest number encodable within a 16-bit unsigned
        // integer.
        if p.len() > std::u16::MAX as usize {
            panic!(ERR_MAX_MESSAGE_LENGTH_EXCEEDED);
        }

        // The full length of the packet is only the packet length, and does
        // NOT include the MAC.
        let full_length = p.len() as u16;

        let mut pkt_len: [u8; LENGTH_HEADER_SIZE] = [0; LENGTH_HEADER_SIZE];
        BigEndian::write_u16(&mut pkt_len, full_length);

        // First, write out the encrypted+MAC'd length prefix for the packet.
        let mut cipher_len = Vec::new();
        let tag = self.send_cipher.encrypt(&[], &mut cipher_len, &pkt_len)?;
        w.write_all(&cipher_len)?;
        w.write_all(&tag)?;

        // Finally, write out the encrypted packet itself. We only write out a
        // single packet, as any fragmentation should have taken place at a
        // higher level.
        let mut cipher_text = Vec::new();
        let tag = self.send_cipher.encrypt(&[], &mut cipher_text, p)?;
        w.write_all(&cipher_text)?;
        w.write_all(&tag)?;
        Ok(())
    }

    // read_message attempts to read the next message from the passed io.Reader. In
    // the case of an authentication error, a non-nil error is returned.
    pub fn read_message<R: io::Read>(&mut self, r: &mut R) -> Result<Vec<u8>, io::Error> {
        r.read_exact(&mut self.next_cipher_header)?;

        // Attempt to decrypt+auth the packet length present in the stream.
        let mut pkt_len_bytes = Vec::new();
        let mut tag: [u8; MAC_SIZE] = [0; MAC_SIZE];
        tag.copy_from_slice(&self.next_cipher_header[LENGTH_HEADER_SIZE..]);
        self.recv_cipher.decrypt(
            &[],
            &mut pkt_len_bytes,
            &self.next_cipher_header[..LENGTH_HEADER_SIZE],
            tag
        )?;

        // Next, using the length read from the packet header, read the
        // encrypted packet itself.
        let pkt_len = BigEndian::read_u16(&pkt_len_bytes) as usize + MAC_SIZE;
        r.read_exact(&mut self.next_cipher_text[..pkt_len])?;

        let mut plaintext = Vec::new();
        let mut tag = [0; MAC_SIZE];
        tag.copy_from_slice(&self.next_cipher_text[pkt_len - MAC_SIZE ..pkt_len]);
        self.recv_cipher.decrypt(
            &[], &mut plaintext,
            &self.next_cipher_text[..pkt_len - MAC_SIZE],
            tag
        )?;

        Ok(plaintext)
    }
}
