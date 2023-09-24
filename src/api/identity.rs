use chrono::Utc;
use num_traits::FromPrimitive;
use rocket::serde::json::{Json, self};
use rocket::{
    form::{Form, FromForm},
    Route,
};
use serde_json::Value;

use crate::{
    api::{
        core::accounts::{PreloginData, RegisterData, _prelogin, _register},
        core::log_user_event,
        core::two_factor::{duo, email, email::EmailTokenData, yubikey},
        ApiResult, EmptyResult, JsonResult, JsonUpcase,
    },
    auth::{generate_organization_api_key_login_claims, ClientHeaders, ClientIp},
    db::{models::*, DbConn},
    error::MapResult,
    mail, util, CONFIG, ldap
};

pub fn routes() -> Vec<Route> {
    routes![login, prelogin, identity_register]
}

#[post("/connect/token", data = "<data>")]
async fn login(data: Form<ConnectData>, client_header: ClientHeaders, mut conn: DbConn) -> JsonResult {
    let data: ConnectData = data.into_inner();

    let mut user_uuid: Option<String> = None;

    let login_result = match data.grant_type.as_ref() {
        "refresh_token" => {
            _check_is_some(&data.refresh_token, "refresh_token cannot be blank")?;
            _refresh_login(data, &mut conn).await
        }
        "ldap_check" => {
            _check_is_some(&data.username, "username cannot be blank")?;
            _check_is_some(&data.password, "password cannot be blank")?;

            _ldap_check(data, &mut conn).await
        }
        "password" => {
            _check_is_some(&data.client_id, "client_id cannot be blank")?;
            _check_is_some(&data.password, "password cannot be blank")?;
            _check_is_some(&data.scope, "scope cannot be blank")?;
            _check_is_some(&data.username, "username cannot be blank")?;

            _check_is_some(&data.device_identifier, "device_identifier cannot be blank")?;
            _check_is_some(&data.device_name, "device_name cannot be blank")?;
            _check_is_some(&data.device_type, "device_type cannot be blank")?;

            _password_login(data, &mut user_uuid, &mut conn, &client_header.ip).await
        }
        "client_credentials" => {
            _check_is_some(&data.client_id, "client_id cannot be blank")?;
            _check_is_some(&data.client_secret, "client_secret cannot be blank")?;
            _check_is_some(&data.scope, "scope cannot be blank")?;

            _check_is_some(&data.device_identifier, "device_identifier cannot be blank")?;
            _check_is_some(&data.device_name, "device_name cannot be blank")?;
            _check_is_some(&data.device_type, "device_type cannot be blank")?;

            _api_key_login(data, &mut user_uuid, &mut conn, &client_header.ip).await
        }
        t => err!("Invalid type", t),
    };

    if let Some(user_uuid) = user_uuid {
        match &login_result {
            Ok(_) => {
                log_user_event(
                    EventType::UserLoggedIn as i32,
                    &user_uuid,
                    client_header.device_type,
                    &client_header.ip.ip,
                    &mut conn,
                )
                .await;
            }
            Err(e) => {
                if let Some(ev) = e.get_event() {
                    log_user_event(
                        ev.event as i32,
                        &user_uuid,
                        client_header.device_type,
                        &client_header.ip.ip,
                        &mut conn,
                    )
                    .await
                }
            }
        }
    }

    login_result
}

async fn _refresh_login(data: ConnectData, conn: &mut DbConn) -> JsonResult {
    // Extract token
    let token = data.refresh_token.unwrap();

    // Get device by refresh token
    let mut device = Device::find_by_refresh_token(&token, conn).await.map_res("Invalid refresh token")?;

    let scope = "api offline_access";
    let scope_vec = vec!["api".into(), "offline_access".into()];

    // Common
    let user = User::find_by_uuid(&device.user_uuid, conn).await.unwrap();
    let orgs = UserOrganization::find_confirmed_by_user(&user.uuid, conn).await;
    let (access_token, expires_in) = device.refresh_tokens(&user, orgs, scope_vec);
    device.save(conn).await?;

    let result = json!({
        "access_token": access_token,
        "expires_in": expires_in,
        "token_type": "Bearer",
        "refresh_token": device.refresh_token,
        "Key": user.akey,
        "PrivateKey": user.private_key,

        "Kdf": user.client_kdf_type,
        "KdfIterations": user.client_kdf_iter,
        "KdfMemory": user.client_kdf_memory,
        "KdfParallelism": user.client_kdf_parallelism,
        "ResetMasterPassword": false, // TODO: according to official server seems something like: user.password_hash.is_empty(), but would need testing
        "scope": scope,
        "unofficialServer": true,
    });

    Ok(Json(result))
}

async fn _ldap_check(
    data: ConnectData,
    conn: &mut DbConn
) -> JsonResult {
    let username = data.username.as_ref().unwrap().trim();
    let ldap_username = username.to_owned() + "@SaigonTechnology"; 

    let option_user = User::find_by_mail(ldap_username.as_str(), conn).await;
    let mut result = json!({
        "status": "error",
        "message": "Something went wrong"
    });
    if option_user.is_some() {
        result = json!({
            "ForcePasswordReset": false
        });
    }
    else if option_user.is_none() {
        result = json!({
            "ForcePasswordReset": true,
        });
    }
    
    return Ok(Json(result));
}

async fn _password_login(
    data: ConnectData,
    user_uuid: &mut Option<String>,
    conn: &mut DbConn,
    ip: &ClientIp,
) -> JsonResult {
    // Validate scope
    let scope = data.scope.as_ref().unwrap();
    if scope != "api offline_access" {
        err!("Scope not supported")
    }
    let scope_vec = vec!["api".into(), "offline_access".into()];

    // Ratelimit the login
    crate::ratelimit::check_limit_login(&ip.ip)?;

    // Get the user
    let username = data.username.as_ref().unwrap().trim();
    if !username.contains("@") {
        return _ldap_login(data, user_uuid, conn, ip).await;
    }

    let mut user = match User::find_by_mail(username, conn).await {
        Some(user) => user,
        None => err!("Username or password is incorrect. Try again", format!("IP: {}. Username: {}.", ip.ip, username)),
    };

    // Set the user_uuid here to be passed back used for event logging.
    *user_uuid = Some(user.uuid.clone());

    // Check password
    let password = data.password.as_ref().unwrap();
    if let Some(auth_request_uuid) = data.auth_request.clone() {
        if let Some(auth_request) = AuthRequest::find_by_uuid(auth_request_uuid.as_str(), conn).await {
            if !auth_request.check_access_code(password) {
                err!(
                    "Username or access code is incorrect. Try again",
                    format!("IP: {}. Username: {}.", ip.ip, username),
                    ErrorEvent {
                        event: EventType::UserFailedLogIn,
                    }
                )
            }
        } else {
            err!(
                "Auth request not found. Try again.",
                format!("IP: {}. Username: {}.", ip.ip, username),
                ErrorEvent {
                    event: EventType::UserFailedLogIn,
                }
            )
        }
    } else if !user.check_valid_password(password) {
        err!(
            "Username or password is incorrect. Try again",
            format!("IP: {}. Username: {}.", ip.ip, username),
            ErrorEvent {
                event: EventType::UserFailedLogIn,
            }
        )
    }

    // Change the KDF Iterations
    if user.password_iterations != CONFIG.password_iterations() {
        user.password_iterations = CONFIG.password_iterations();
        user.set_password(password, None, false, None);

        if let Err(e) = user.save(conn).await {
            error!("Error updating user: {:#?}", e);
        }
    }

    // Check if the user is disabled
    if !user.enabled {
        err!(
            "This user has been disabled",
            format!("IP: {}. Username: {}.", ip.ip, username),
            ErrorEvent {
                event: EventType::UserFailedLogIn
            }
        )
    }

    let now = Utc::now().naive_utc();

    if user.verified_at.is_none() && CONFIG.mail_enabled() && CONFIG.signups_verify() {
        if user.last_verifying_at.is_none()
            || now.signed_duration_since(user.last_verifying_at.unwrap()).num_seconds()
                > CONFIG.signups_verify_resend_time() as i64
        {
            let resend_limit = CONFIG.signups_verify_resend_limit() as i32;
            if resend_limit == 0 || user.login_verify_count < resend_limit {
                // We want to send another email verification if we require signups to verify
                // their email address, and we haven't sent them a reminder in a while...
                user.last_verifying_at = Some(now);
                user.login_verify_count += 1;

                if let Err(e) = user.save(conn).await {
                    error!("Error updating user: {:#?}", e);
                }

                if let Err(e) = mail::send_verify_email(&user.email, &user.uuid).await {
                    error!("Error auto-sending email verification email: {:#?}", e);
                }
            }
        }

        // We still want the login to fail until they actually verified the email address
        err!(
            "Please verify your email before trying again.",
            format!("IP: {}. Username: {}.", ip.ip, username),
            ErrorEvent {
                event: EventType::UserFailedLogIn
            }
        )
    }

    let (mut device, new_device) = get_device(&data, conn, &user).await;

    let twofactor_token = twofactor_auth(&user.uuid, &data, &mut device, ip, conn).await?;

    if CONFIG.mail_enabled() && new_device {
        if let Err(e) = mail::send_new_device_logged_in(&user.email, &ip.ip.to_string(), &now, &device.name).await {
            error!("Error sending new device email: {:#?}", e);

            if CONFIG.require_device_email() {
                err!(
                    "Could not send login notification email. Please contact your administrator.",
                    ErrorEvent {
                        event: EventType::UserFailedLogIn
                    }
                )
            }
        }
    }

    // Common
    let orgs = UserOrganization::find_confirmed_by_user(&user.uuid, conn).await;
    let (access_token, expires_in) = device.refresh_tokens(&user, orgs, scope_vec);
    device.save(conn).await?;

    let mut result = json!({
        "access_token": access_token,
        "expires_in": expires_in,
        "token_type": "Bearer",
        "refresh_token": device.refresh_token,
        "Key": user.akey,
        "PrivateKey": user.private_key,
        //"TwoFactorToken": "11122233333444555666777888999"

        "Kdf": user.client_kdf_type,
        "KdfIterations": user.client_kdf_iter,
        "KdfMemory": user.client_kdf_memory,
        "KdfParallelism": user.client_kdf_parallelism,
        "ResetMasterPassword": false,// TODO: Same as above
        "scope": scope,
        "unofficialServer": true,
        "UserDecryptionOptions": {
            "HasMasterPassword": !user.password_hash.is_empty(),
            "Object": "userDecryptionOptions"
        },
    });

    if let Some(token) = twofactor_token {
        result["TwoFactorToken"] = Value::String(token);
    }

    info!("User {} logged in successfully. IP: {}", username, ip.ip);
    Ok(Json(result))
}

async fn _ldap_login(
    data: ConnectData,
    user_uuid: &mut Option<String>,
    conn: &mut DbConn,
    ip: &ClientIp, 
) -> JsonResult {
    let username = data.username.as_ref().unwrap().trim();
    let password = data.password.as_ref().unwrap();
    let user: User;
    let now = Utc::now().naive_utc();
    let is_ldap_auth = ldap::sync_user_with_ldap(username, password).await;
    if !is_ldap_auth {
        err!("Username or password is incorre`ct. Try again", format!("IP: {}. Username: {}.", ip.ip, username))
    }
    let scope = data.scope.as_ref().unwrap();
    if scope != "api offline_access" {
        err!("Scope not supported")
    }
    let scope_vec = vec!["api".into(), "offline_access".into()];
    let ldap_username = username.to_owned() + "@SaigonTechnology";
    let option_user =  User::find_by_mail(ldap_username.as_str(), conn).await;
    if option_user.is_some() {
        user = option_user.unwrap();
    }
    else if option_user.is_none() {
        let mut new_user = User::new((*username).to_string());
        new_user.client_kdf_type = 0;
        new_user.client_kdf_iter = 600000;
        //TODO: add key
        new_user.akey = String::from("2.YRFiUS+p1J0X2nUIHCAiYQ==|rpettSy5r8QsYr2sycnw94w83h1K/siTlHngpeK/W31eKSJGgLePlAxQH/nQ+o/wF1/nkl2C5qX9U/PArOVTUuGFUQK9ZDWeyIg6Ez/OEzY=|zO/U9MZwQ9GrDvCYpemc1Q52rfL1CVmKQEQPKPrIGTE=");
        new_user.private_key = Some(String::from("2.ee3oVD8A8CHDp3Ez6OT0cQ==|8MIGnxPgJVysg634ANrx05AJUGWRMDWvnnqvEuNutgoEnmiZ9tnDSeji5YNUp8PL5dH1ziFybe7AHkKely+d3072N0c5wR6hKUudDJij/WXB8IFm37c8uo+GU33nc0Vy6MqCaWEOUnrn3r8u4VUnDkxoCpETUrGOpGYQJ75a1sjVCAmigxxRWJ2lRP+5SRneYw5n8Wk+wigsqLi9X7meoyqRMTzRKErTkXj8egicEackxET9yXvJzPhOhsjAGnsTIWN7uLXE0aViOaM8yFFtfmpf+1mnu0Jj6ZSto/vv/SuCgjFErsySB7M4Locise/53+53BjgUnT+majSTqZ6RQjxeNqKaF/UqECE9ZpKjcqMKWmZI9L6Lw9CMOUm8RWTl2DumowCT1K/dKtSXJe5ZRbk12A9qeHRkqgqcufS5DB/SAej7XNXNhTK15GVd/qfhWkOnxM+OFrOYYHcxJ3kIE+ImS9WUYfZ5yEYvN3ZhFkTnE7GUmgZwbW5n6rfmAw/lWLNYvx5DvH5fpi6HkEw7nfTYWNsNyZ+zG6EWabhRHIsAXRtgXPSaq/qD2I/dpTv464OCRx3pwCe1bf/WjC/BzrDHADfYPirflcB29C3KQKYiIFiufLFVBO/VB6w0FRZ3yEY3uJYCfZmXkP1EF6viYMLh6pLgU84OsxIbR2n7/EdvpdJXrS77KwEN6ZW6lDKA9M2haXEzfzyqfkR/h8Dsb6Yx0ZBS9gXPuoB5Mpyq4MaZz/MWjUJ1SgQ4gNaxtQKseXUngexEKlmXO5IcnuztD80hARBmjVka3N6Y2v2UWVFl2kyRsp6c4LxIc6GBT4m0Bg49YcIfgzdzgqPKnD8mpGJAWZPHCsEI0KgzWdQBPBjQM9s7s+Id9iGli7zHoLBE3pQChYpoTeNIiQl8nv+j0eCIyTGMal4VrCf6QN1HmtsEyf0BlJr+knqeyVXmi8ljN9sBznKAdO/I9lQNEZUt6YYJNU4RyPkuvvxeu3huyIP6Akp0vDafPI9dyHEbaiQUQsZwY9fP7rrCplAb8hFUCS8F1EyGXKImTAzglewY95m6idCO2qbMKKHQp+4khgA7+E9sU0T9uy0E4LYD6nbrCJUWP9aulL5+ImNaDqrB+pcWpVFCo+FagdOTxJ3Um4VPEsyQS1Ao1I1l7fF2H8Uy2eCs5leOk1CLfNCdfwgNywtUQsSRzfKEkI7FSwRQ0lSt0GP7NtN3wygMPQAzbqNQup4RvPNcLVBCSwObVWT+ZzFhqaninjqf8lg7UnImdT8w8l8EzCViYKluAv0QQyAZH7onGYZDif6QX7BJpn5A9AdWhmx1O9r5FU2zaVJu1FRd/RShWSQhDmTRMz74SVEg7ZoW0/IpqNDPcYTNqoEmV4S5Sqco2lR19dYcFrpnTkjKcFYVXYjn8E4Y0FrU69lGzfZc2CF/fDw8zZAfs0yVMOUDRd46dYoYhvhrauYUB/R8aPCNAUHkPqKof3eMJjy97Xci6JbsMpNdBotGTjp1jiFGML859sB0/oeJjdvQBOzoZqcGq8Wk4yFOjtTAfd3TJF01s5Z1VGww3QbPAhB7jBP+PdwIIi1UZH9Zv4uCaeyAKMe0K5N3Wb+aBLkzRgR/AXwUAML3Gf4WA2A5DwzWHTY=|6JZHe+UO3fKsl/KcmkDPyoWEJUOkisehD+q7rOc1HIw="));
        new_user.public_key = Some(String::from("MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAu8MKt+iUkaFbpKVfwMET2vC1MvrGSFqP/R9l+B35lBuD1oa9I8XrGWMNJuhtLkyJWZfmGmV4POlDbKJU/7l/a/3tYJuBfFqyyo1uPMTPc78PbExg6+FeWDDu4itPTLh3S7fUxUYOona5bdNEWnyOYoSUuRrei1gmGN7DJX+9NqjWQrcX8ZDM2AD8cwRbHwdRvWYQMLv7d0wivyrCpRxXVhfe3sBrBMPYfnBKgikZ3gZWxr9IPp50UeSWZYXnj6JSTxWCrTJDKtL9E1XgFxrJ/81OC9T1HKx+BH/LBEMT3gRBXZgTvEseHXejr4d1CLYTvrBzn75VpBdLiOpJEpAn9QIDAQAB"));
        new_user.save(conn).await?;
        user = new_user;
    }
    else {
        err!("Username or password is incorrect. Try again", format!("IP: {}. Username: {}.", ip.ip, username))
    }
    *user_uuid = Some(user.uuid.clone());

    let (mut device, new_device) = get_device(&data, conn, &user).await;

    let twofactor_token = twofactor_auth(&user.uuid, &data, &mut device, ip, conn).await?;

    if CONFIG.mail_enabled() && new_device {
        if let Err(e) = mail::send_new_device_logged_in(&user.email, &ip.ip.to_string(), &now, &device.name).await {
            error!("Error sending new device email: {:#?}", e);

            if CONFIG.require_device_email() {
                err!(
                    "Could not send login notification email. Please contact your administrator.",
                    ErrorEvent {
                        event: EventType::UserFailedLogIn
                    }
                )
            }
        }
    }

    // Common
    let orgs = UserOrganization::find_confirmed_by_user(&user.uuid, conn).await;
    let (access_token, expires_in) = device.refresh_tokens(&user, orgs, scope_vec);
    device.save(conn).await?;

    let mut result = json!({
        "access_token": access_token,
        "expires_in": expires_in,
        "token_type": "Bearer",
        "refresh_token": device.refresh_token,
        "Key": user.akey,
        "PrivateKey": user.private_key,
        //"TwoFactorToken": "11122233333444555666777888999"

        "Kdf": user.client_kdf_type,
        "KdfIterations": user.client_kdf_iter,
        "KdfMemory": user.client_kdf_memory,
        "KdfParallelism": user.client_kdf_parallelism,
        "ResetMasterPassword": false,// TODO: Same as above
        "scope": scope,
        "unofficialServer": true,
    });

    if let Some(token) = twofactor_token {
        result["TwoFactorToken"] = Value::String(token);
    }

    info!("User {} logged in successfully. IP: {}", username, ip.ip);
    Ok(Json(result))
}

async fn _api_key_login(
    data: ConnectData,
    user_uuid: &mut Option<String>,
    conn: &mut DbConn,
    ip: &ClientIp,
) -> JsonResult {
    // Ratelimit the login
    crate::ratelimit::check_limit_login(&ip.ip)?;

    // Validate scope
    match data.scope.as_ref().unwrap().as_ref() {
        "api" => _user_api_key_login(data, user_uuid, conn, ip).await,
        "api.organization" => _organization_api_key_login(data, conn, ip).await,
        _ => err!("Scope not supported"),
    }
}

async fn _user_api_key_login(
    data: ConnectData,
    user_uuid: &mut Option<String>,
    conn: &mut DbConn,
    ip: &ClientIp,
) -> JsonResult {
    // Get the user via the client_id
    let client_id = data.client_id.as_ref().unwrap();
    let client_user_uuid = match client_id.strip_prefix("user.") {
        Some(uuid) => uuid,
        None => err!("Malformed client_id", format!("IP: {}.", ip.ip)),
    };
    let user = match User::find_by_uuid(client_user_uuid, conn).await {
        Some(user) => user,
        None => err!("Invalid client_id", format!("IP: {}.", ip.ip)),
    };

    // Set the user_uuid here to be passed back used for event logging.
    *user_uuid = Some(user.uuid.clone());

    // Check if the user is disabled
    if !user.enabled {
        err!(
            "This user has been disabled (API key login)",
            format!("IP: {}. Username: {}.", ip.ip, user.email),
            ErrorEvent {
                event: EventType::UserFailedLogIn
            }
        )
    }

    // Check API key. Note that API key logins bypass 2FA.
    let client_secret = data.client_secret.as_ref().unwrap();
    if !user.check_valid_api_key(client_secret) {
        err!(
            "Incorrect client_secret",
            format!("IP: {}. Username: {}.", ip.ip, user.email),
            ErrorEvent {
                event: EventType::UserFailedLogIn
            }
        )
    }

    let (mut device, new_device) = get_device(&data, conn, &user).await;

    if CONFIG.mail_enabled() && new_device {
        let now = Utc::now().naive_utc();
        if let Err(e) = mail::send_new_device_logged_in(&user.email, &ip.ip.to_string(), &now, &device.name).await {
            error!("Error sending new device email: {:#?}", e);

            if CONFIG.require_device_email() {
                err!(
                    "Could not send login notification email. Please contact your administrator.",
                    ErrorEvent {
                        event: EventType::UserFailedLogIn
                    }
                )
            }
        }
    }

    // Common
    let scope_vec = vec!["api".into()];
    let orgs = UserOrganization::find_confirmed_by_user(&user.uuid, conn).await;
    let (access_token, expires_in) = device.refresh_tokens(&user, orgs, scope_vec);
    device.save(conn).await?;

    info!("User {} logged in successfully via API key. IP: {}", user.email, ip.ip);

    // Note: No refresh_token is returned. The CLI just repeats the
    // client_credentials login flow when the existing token expires.
    let result = json!({
        "access_token": access_token,
        "expires_in": expires_in,
        "token_type": "Bearer",
        "Key": user.akey,
        "PrivateKey": user.private_key,

        "Kdf": user.client_kdf_type,
        "KdfIterations": user.client_kdf_iter,
        "KdfMemory": user.client_kdf_memory,
        "KdfParallelism": user.client_kdf_parallelism,
        "ResetMasterPassword": false, // TODO: Same as above
        "scope": "api",
        "unofficialServer": true,
    });

    Ok(Json(result))
}

async fn _organization_api_key_login(data: ConnectData, conn: &mut DbConn, ip: &ClientIp) -> JsonResult {
    // Get the org via the client_id
    let client_id = data.client_id.as_ref().unwrap();
    let org_uuid = match client_id.strip_prefix("organization.") {
        Some(uuid) => uuid,
        None => err!("Malformed client_id", format!("IP: {}.", ip.ip)),
    };
    let org_api_key = match OrganizationApiKey::find_by_org_uuid(org_uuid, conn).await {
        Some(org_api_key) => org_api_key,
        None => err!("Invalid client_id", format!("IP: {}.", ip.ip)),
    };

    // Check API key.
    let client_secret = data.client_secret.as_ref().unwrap();
    if !org_api_key.check_valid_api_key(client_secret) {
        err!("Incorrect client_secret", format!("IP: {}. Organization: {}.", ip.ip, org_api_key.org_uuid))
    }

    let claim = generate_organization_api_key_login_claims(org_api_key.uuid, org_api_key.org_uuid);
    let access_token = crate::auth::encode_jwt(&claim);

    Ok(Json(json!({
        "access_token": access_token,
        "expires_in": 3600,
        "token_type": "Bearer",
        "scope": "api.organization",
        "unofficialServer": true,
    })))
}

/// Retrieves an existing device or creates a new device from ConnectData and the User
async fn get_device(data: &ConnectData, conn: &mut DbConn, user: &User) -> (Device, bool) {
    // On iOS, device_type sends "iOS", on others it sends a number
    // When unknown or unable to parse, return 14, which is 'Unknown Browser'
    let device_type = util::try_parse_string(data.device_type.as_ref()).unwrap_or(14);
    let device_id = data.device_identifier.clone().expect("No device id provided");
    let device_name = data.device_name.clone().expect("No device name provided");

    let mut new_device = false;
    // Find device or create new
    let device = match Device::find_by_uuid_and_user(&device_id, &user.uuid, conn).await {
        Some(device) => device,
        None => {
            new_device = true;
            Device::new(device_id, user.uuid.clone(), device_name, device_type)
        }
    };

    (device, new_device)
}

async fn twofactor_auth(
    user_uuid: &str,
    data: &ConnectData,
    device: &mut Device,
    ip: &ClientIp,
    conn: &mut DbConn,
) -> ApiResult<Option<String>> {
    let twofactors = TwoFactor::find_by_user(user_uuid, conn).await;

    // No twofactor token if twofactor is disabled
    if twofactors.is_empty() {
        return Ok(None);
    }

    TwoFactorIncomplete::mark_incomplete(user_uuid, &device.uuid, &device.name, ip, conn).await?;

    let twofactor_ids: Vec<_> = twofactors.iter().map(|tf| tf.atype).collect();
    let selected_id = data.two_factor_provider.unwrap_or(twofactor_ids[0]); // If we aren't given a two factor provider, asume the first one

    let twofactor_code = match data.two_factor_token {
        Some(ref code) => code,
        None => err_json!(_json_err_twofactor(&twofactor_ids, user_uuid, conn).await?, "2FA token not provided"),
    };

    let selected_twofactor = twofactors.into_iter().find(|tf| tf.atype == selected_id && tf.enabled);

    use crate::api::core::two_factor as _tf;
    use crate::crypto::ct_eq;

    let selected_data = _selected_data(selected_twofactor);
    let mut remember = data.two_factor_remember.unwrap_or(0);

    match TwoFactorType::from_i32(selected_id) {
        Some(TwoFactorType::Authenticator) => {
            _tf::authenticator::validate_totp_code_str(user_uuid, twofactor_code, &selected_data?, ip, conn).await?
        }
        Some(TwoFactorType::Webauthn) => {
            _tf::webauthn::validate_webauthn_login(user_uuid, twofactor_code, conn).await?
        }
        Some(TwoFactorType::YubiKey) => _tf::yubikey::validate_yubikey_login(twofactor_code, &selected_data?).await?,
        Some(TwoFactorType::Duo) => {
            _tf::duo::validate_duo_login(data.username.as_ref().unwrap().trim(), twofactor_code, conn).await?
        }
        Some(TwoFactorType::Email) => {
            _tf::email::validate_email_code_str(user_uuid, twofactor_code, &selected_data?, conn).await?
        }

        Some(TwoFactorType::Remember) => {
            match device.twofactor_remember {
                Some(ref code) if !CONFIG.disable_2fa_remember() && ct_eq(code, twofactor_code) => {
                    remember = 1; // Make sure we also return the token here, otherwise it will only remember the first time
                }
                _ => {
                    err_json!(
                        _json_err_twofactor(&twofactor_ids, user_uuid, conn).await?,
                        "2FA Remember token not provided"
                    )
                }
            }
        }
        _ => err!(
            "Invalid two factor provider",
            ErrorEvent {
                event: EventType::UserFailedLogIn2fa
            }
        ),
    }

    TwoFactorIncomplete::mark_complete(user_uuid, &device.uuid, conn).await?;

    if !CONFIG.disable_2fa_remember() && remember == 1 {
        Ok(Some(device.refresh_twofactor_remember()))
    } else {
        device.delete_twofactor_remember();
        Ok(None)
    }
}

fn _selected_data(tf: Option<TwoFactor>) -> ApiResult<String> {
    tf.map(|t| t.data).map_res("Two factor doesn't exist")
}

async fn _json_err_twofactor(providers: &[i32], user_uuid: &str, conn: &mut DbConn) -> ApiResult<Value> {
    use crate::api::core::two_factor;

    let mut result = json!({
        "error" : "invalid_grant",
        "error_description" : "Two factor required.",
        "TwoFactorProviders" : providers,
        "TwoFactorProviders2" : {} // { "0" : null }
    });

    for provider in providers {
        result["TwoFactorProviders2"][provider.to_string()] = Value::Null;

        match TwoFactorType::from_i32(*provider) {
            Some(TwoFactorType::Authenticator) => { /* Nothing to do for TOTP */ }

            Some(TwoFactorType::Webauthn) if CONFIG.domain_set() => {
                let request = two_factor::webauthn::generate_webauthn_login(user_uuid, conn).await?;
                result["TwoFactorProviders2"][provider.to_string()] = request.0;
            }

            Some(TwoFactorType::Duo) => {
                let email = match User::find_by_uuid(user_uuid, conn).await {
                    Some(u) => u.email,
                    None => err!("User does not exist"),
                };

                let (signature, host) = duo::generate_duo_signature(&email, conn).await?;

                result["TwoFactorProviders2"][provider.to_string()] = json!({
                    "Host": host,
                    "Signature": signature,
                });
            }

            Some(tf_type @ TwoFactorType::YubiKey) => {
                let twofactor = match TwoFactor::find_by_user_and_type(user_uuid, tf_type as i32, conn).await {
                    Some(tf) => tf,
                    None => err!("No YubiKey devices registered"),
                };

                let yubikey_metadata: yubikey::YubikeyMetadata = serde_json::from_str(&twofactor.data)?;

                result["TwoFactorProviders2"][provider.to_string()] = json!({
                    "Nfc": yubikey_metadata.Nfc,
                })
            }

            Some(tf_type @ TwoFactorType::Email) => {
                use crate::api::core::two_factor as _tf;

                let twofactor = match TwoFactor::find_by_user_and_type(user_uuid, tf_type as i32, conn).await {
                    Some(tf) => tf,
                    None => err!("No twofactor email registered"),
                };

                // Send email immediately if email is the only 2FA option
                if providers.len() == 1 {
                    _tf::email::send_token(user_uuid, conn).await?
                }

                let email_data = EmailTokenData::from_json(&twofactor.data)?;
                result["TwoFactorProviders2"][provider.to_string()] = json!({
                    "Email": email::obscure_email(&email_data.email),
                })
            }

            _ => {}
        }
    }

    Ok(result)
}

#[post("/accounts/prelogin", data = "<data>")]
async fn prelogin(data: JsonUpcase<PreloginData>, conn: DbConn) -> Json<Value> {
    _prelogin(data, conn).await
}

#[post("/accounts/register", data = "<data>")]
async fn identity_register(data: JsonUpcase<RegisterData>, conn: DbConn) -> JsonResult {
    _register(data, conn).await
}

// https://github.com/bitwarden/jslib/blob/master/common/src/models/request/tokenRequest.ts
// https://github.com/bitwarden/mobile/blob/master/src/Core/Models/Request/TokenRequest.cs
#[derive(Debug, Clone, Default, FromForm)]
#[allow(non_snake_case)]
struct ConnectData {
    #[field(name = uncased("grant_type"))]
    #[field(name = uncased("granttype"))]
    grant_type: String, // refresh_token, password, client_credentials (API key)

    // Needed for grant_type="refresh_token"
    #[field(name = uncased("refresh_token"))]
    #[field(name = uncased("refreshtoken"))]
    refresh_token: Option<String>,

    // Needed for grant_type = "password" | "client_credentials"
    #[field(name = uncased("client_id"))]
    #[field(name = uncased("clientid"))]
    client_id: Option<String>, // web, cli, desktop, browser, mobile
    #[field(name = uncased("client_secret"))]
    #[field(name = uncased("clientsecret"))]
    client_secret: Option<String>,
    #[field(name = uncased("password"))]
    password: Option<String>,
    #[field(name = uncased("scope"))]
    scope: Option<String>,
    #[field(name = uncased("username"))]
    username: Option<String>,

    #[field(name = uncased("device_identifier"))]
    #[field(name = uncased("deviceidentifier"))]
    device_identifier: Option<String>,
    #[field(name = uncased("device_name"))]
    #[field(name = uncased("devicename"))]
    device_name: Option<String>,
    #[field(name = uncased("device_type"))]
    #[field(name = uncased("devicetype"))]
    device_type: Option<String>,
    #[allow(unused)]
    #[field(name = uncased("device_push_token"))]
    #[field(name = uncased("devicepushtoken"))]
    _device_push_token: Option<String>, // Unused; mobile device push not yet supported.

    // Needed for two-factor auth
    #[field(name = uncased("two_factor_provider"))]
    #[field(name = uncased("twofactorprovider"))]
    two_factor_provider: Option<i32>,
    #[field(name = uncased("two_factor_token"))]
    #[field(name = uncased("twofactortoken"))]
    two_factor_token: Option<String>,
    #[field(name = uncased("two_factor_remember"))]
    #[field(name = uncased("twofactorremember"))]
    two_factor_remember: Option<i32>,
    #[field(name = uncased("authrequest"))]
    auth_request: Option<String>,
}

fn _check_is_some<T>(value: &Option<T>, msg: &str) -> EmptyResult {
    if value.is_none() {
        err!(msg)
    }
    Ok(())
}
