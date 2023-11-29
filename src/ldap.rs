use base64;
use ldap3::LdapConnAsync;
use pbkdf2::pbkdf2_hmac;
use sha2::Sha256;
use std::env;

pub async fn sync_user_with_ldap(username: &str, password: &String) -> Option<String> {
    let ldap_url = match env::var("LDAP_URL") {
        Ok(v) => v,
        Err(e) => {
            warn!("LDAP_URL not set");
            return None;
        }
    };
    let (conn, mut ldap) = match LdapConnAsync::new(ldap_url.as_str()).await {
        Ok((conn, ldap)) => (conn, ldap),
        Err(e) => {
            warn!("Error connecting to ldap: {}", e);
            return None;
        }
    };
    ldap3::drive!(conn);
    let ldap_username = username.replace("@saigontechnology", "");
    info!("Ldap username: {}", ldap_username);
    let result = ldap.simple_bind(&ldap_username, password).await.unwrap().success();

    let _ = ldap.unbind().await;
    if !result.is_err() {
        return Some(create_master_key(username, password));
    }
    return None;
}

pub fn create_master_key(email: &str, pass: &String) -> String {
    let password = env::var("MASTER_KEY").unwrap_or_else(|_| pass.to_owned());
    let salt = email.as_bytes();
    let iterations = 600_000;
    let mut key = [0u8; 32];
    let mut key_hash = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, iterations, &mut key);
    pbkdf2_hmac::<Sha256>(&key, &pass.to_owned().as_bytes(), 2, &mut key_hash);
    return base64::encode(&key) + "." + &base64::encode(&key_hash);
}
pub fn rsa_decrypt(private_key: &[u8], data: &[u8]) -> Vec<u8> {
    return Vec::new();
}
// pub fn rsa_encrypt(private_key: vec<u8>, data: vec<u8>) -> vec[u8] {
//
// }
//
// pub fn make_key(password: &[u8], salt: &[u8]) -> [u8; 20] {
//     let iter = 600_000;
//     let mut key1 = [0u8; 20];
//     pbkdf2_hmac::<Sha256>(password, salt, iter, &mut key1);
//     return key1;
// }

// pub fn make_enc_key(key: &[u8]) -> String {
//     let mut rng = rand::thread_rng();
//     let enc_key = rng.gen::<[u8; 64]>();
//     return String::from(enc_key);
// }

// pub fn from_utf8_to_array(str: &str) -> Vec<u8> {
//     return str.bytes().collect()
// }
