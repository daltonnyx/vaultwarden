use hex_literal::hex;
use ldap3::LdapConnAsync;
use pbkdf2::pbkdf2_hmac;
use rand::Rng;
use sha2::Sha256;

use std::convert::TryInto;

pub async fn sync_user_with_ldap(username: &str, password: &String) -> bool {
    return true;
    let (conn, mut ldap) = match LdapConnAsync::new("ldap://10.30.20.5:389").await {
        Ok((conn, ldap)) => (conn, ldap),
        Err(e) => {
            warn!("Error connecting to ldap: {}", e);
            return false;
        }
    };
    ldap3::drive!(conn);
    let ldap_username = username.replace("@saigontechnology", "");
    info!("Ldap username: {}", ldap_username);
    let result = ldap.simple_bind(&ldap_username, password).await.unwrap().success();

    let mut is_authenticated = false;
    if !result.is_err() {
        is_authenticated = true;
    }
    let _ = ldap.unbind().await;
    return is_authenticated;
}

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
