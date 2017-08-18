// Copyright 2016 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement.  This, along with the Licenses can be
// found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

use AccessContainerEntry;
use AuthError;
use Authenticator;
use app_auth::{AppState, app_state};
use app_container;
use config;
use ffi_utils::{FFI_RESULT_OK, FfiResult, OpaqueCtx, SafePtr, catch_unwind_cb, from_c_str,
                vec_into_raw_parts};
use futures::Future;
use maidsafe_utilities::serialisation::deserialise;
use routing::User::Key;
use routing::XorName;
use rust_sodium::crypto::sign::PublicKey;
use safe_core::FutureExt;
use safe_core::ffi::XorNameArray;
use safe_core::ipc::{IpcError, access_container_enc_key};
use safe_core::ipc::req::{AppExchangeInfo, containers_into_vec};
use safe_core::ipc::req::ffi::{self, ContainerPermissions};
use safe_core::ipc::resp::AppAccess;
use safe_core::ipc::resp::ffi::AppAccess as FfiAppAccess;
use safe_core::utils::symmetric_decrypt;
use std::collections::HashMap;
use std::os::raw::{c_char, c_void};

/// Application registered in the authenticator
#[repr(C)]
pub struct RegisteredApp {
    /// Unique application identifier
    pub app_info: ffi::AppExchangeInfo,
    /// List of containers that this application has access to
    pub containers: *const ContainerPermissions,
    /// Length of the containers array
    pub containers_len: usize,
    /// Capacity of the containers array. Internal data required
    /// for the Rust allocator.
    pub containers_cap: usize,
}

impl Drop for RegisteredApp {
    fn drop(&mut self) {
        unsafe {
            let _ = Vec::from_raw_parts(
                self.containers as *mut ContainerPermissions,
                self.containers_len,
                self.containers_cap,
            );
        }
    }
}

/// Removes a revoked app from the authenticator config
#[no_mangle]
pub unsafe extern "C" fn auth_rm_revoked_app(
    auth: *const Authenticator,
    app_id: *const c_char,
    user_data: *mut c_void,
    o_cb: extern "C" fn(*mut c_void, FfiResult),
) {

    let user_data = OpaqueCtx(user_data);

    catch_unwind_cb(user_data.0, o_cb, || -> Result<_, AuthError> {
        let app_id = from_c_str(app_id)?;
        let app_id2 = app_id.clone();
        let app_id3 = app_id.clone();

        (*auth).send(move |client| {
            let c2 = client.clone();
            let c3 = client.clone();
            let c4 = client.clone();

            config::list_apps(client)
                .and_then(move |(apps_version, apps)| {
                    app_state(&c2, &apps, &app_id).map(move |app_state| {
                        (app_state, apps, apps_version)
                    })
                })
                .and_then(move |(app_state, apps, apps_version)| match app_state {
                    AppState::Revoked => Ok((apps, apps_version)),
                    AppState::Authenticated => Err(AuthError::from("App is not revoked")),
                    AppState::NotAuthenticated => Err(AuthError::IpcError(IpcError::UnknownApp)),
                })
                .and_then(move |(apps, apps_version)| {
                    config::remove_app(&c3, apps, config::next_version(apps_version), &app_id2)
                })
                .and_then(move |_| app_container::remove(c4, &app_id3))
                .then(move |res| {
                    call_result_cb!(res, user_data, o_cb);
                    Ok(())
                })
                .into_box()
                .into()
        })
    });
}

/// Get a list of apps revoked from authenticator
pub unsafe extern "C" fn auth_revoked_apps(
    auth: *const Authenticator,
    user_data: *mut c_void,
    o_cb: extern "C" fn(*mut c_void, FfiResult, *const ffi::AppExchangeInfo, usize),
) {
    let user_data = OpaqueCtx(user_data);

    catch_unwind_cb(user_data.0, o_cb, || -> Result<_, AuthError> {
        (*auth).send(move |client| {
            let c2 = client.clone();
            let c3 = client.clone();

            config::list_apps(client)
                .and_then(move |(_, auth_cfg)| {
                    c2.access_container().map_err(AuthError::from).map(
                        move |access_container| (access_container, auth_cfg),
                    )
                })
                .and_then(move |(access_container, auth_cfg)| {
                    c3.list_mdata_entries(access_container.name, access_container.type_tag)
                        .map_err(From::from)
                        .map(move |entries| (access_container, entries, auth_cfg))
                })
                .and_then(move |(access_container, entries, auth_cfg)| {
                    let mut apps = Vec::new();
                    let nonce = access_container.nonce().ok_or_else(|| {
                        AuthError::from("No nonce on access container's MDataInfo")
                    })?;

                    for app in auth_cfg.values() {
                        let key = access_container_enc_key(&app.info.id, &app.keys.enc_key, nonce)?;

                        // If the app is not in the access container, or if the app entry has
                        // been deleted (is empty), then it's revoked.
                        let revoked = entries
                            .get(&key)
                            .map(|entry| entry.content.is_empty())
                            .unwrap_or(true);

                        if revoked {
                            apps.push(app.info.clone().into_repr_c()?);
                        }
                    }

                    o_cb(user_data.0, FFI_RESULT_OK, apps.as_safe_ptr(), apps.len());

                    Ok(())
                })
                .map_err(move |e| {
                    call_result_cb!(Err::<(), _>(e), user_data, o_cb);
                })
                .into_box()
                .into()
        })?;

        Ok(())
    })
}

/// Get a list of apps registered in authenticator
#[no_mangle]
pub unsafe extern "C" fn auth_registered_apps(
    auth: *const Authenticator,
    user_data: *mut c_void,
    o_cb: extern "C" fn(*mut c_void, FfiResult, *const RegisteredApp, usize),
) {
    let user_data = OpaqueCtx(user_data);

    catch_unwind_cb(user_data.0, o_cb, || -> Result<_, AuthError> {
        (*auth).send(move |client| {
            let c2 = client.clone();
            let c3 = client.clone();

            config::list_apps(client)
                .and_then(move |(_, auth_cfg)| {
                    c2.access_container().map_err(AuthError::from).map(
                        move |access_container| (access_container, auth_cfg),
                    )
                })
                .and_then(move |(access_container, auth_cfg)| {
                    c3.list_mdata_entries(access_container.name, access_container.type_tag)
                        .map_err(From::from)
                        .map(move |entries| (access_container, entries, auth_cfg))
                })
                .and_then(move |(access_container, entries, auth_cfg)| {
                    let mut apps = Vec::new();

                    let nonce = access_container.nonce().ok_or_else(|| {
                        AuthError::from("No nonce on access container's MDataInfo")
                    })?;

                    for app in auth_cfg.values() {
                        let key = access_container_enc_key(&app.info.id, &app.keys.enc_key, nonce)?;

                        // Empty entry means it has been deleted.
                        let entry = match entries.get(&key) {
                            Some(entry) if !entry.content.is_empty() => Some(entry),
                            _ => None,
                        };

                        if let Some(entry) = entry {
                            let plaintext = symmetric_decrypt(&entry.content, &app.keys.enc_key)?;
                            let app_access = deserialise::<AccessContainerEntry>(&plaintext)?;

                            let containers =
                                containers_into_vec(
                                    app_access.into_iter().map(|(key, (_, perms))| (key, perms)),
                                )?;

                            let (containers_ptr, len, cap) = vec_into_raw_parts(containers);
                            let reg_app = RegisteredApp {
                                app_info: app.info.clone().into_repr_c()?,
                                containers: containers_ptr,
                                containers_len: len,
                                containers_cap: cap,
                            };

                            apps.push(reg_app);
                        }
                    }

                    o_cb(user_data.0, FFI_RESULT_OK, apps.as_safe_ptr(), apps.len());

                    Ok(())
                })
                .map_err(move |e| {
                    call_result_cb!(Err::<(), _>(e), user_data, o_cb);
                })
                .into_box()
                .into()
        })?;

        Ok(())
    })
}

/// Return a list of apps having access to an arbitrary MD object.
/// `md_name` and `md_type_tag` together correspond to a single MD.
#[no_mangle]
pub unsafe extern "C" fn auth_apps_accessing_mutable_data(
    auth: *mut Authenticator,
    md_name: *const XorNameArray,
    md_type_tag: u64,
    user_data: *mut c_void,
    o_cb: extern "C" fn(*mut c_void, FfiResult, *const FfiAppAccess, usize),
) {
    let user_data = OpaqueCtx(user_data);
    let name = XorName(*md_name);

    catch_unwind_cb(user_data.0, o_cb, || -> Result<_, AuthError> {
        (*auth).send(move |client| {
            let c2 = client.clone();

            client
                .list_mdata_permissions(name, md_type_tag)
                .map_err(AuthError::from)
                .join(
                    // Fetch a list of registered apps in parallel
                    config::list_apps(&c2).map(|(_, apps)| {
                        // Convert the HashMap keyed by id to one keyed by public key
                        apps.into_iter()
                            .map(|(_, app_info)| (app_info.keys.owner_key, app_info.info))
                            .collect::<HashMap<PublicKey, AppExchangeInfo>>()
                    }),
                )
                .and_then(move |(permissions, apps)| {
                    // Map the list of keys retrieved from MD to a list of registered apps (even if
                    // they're in the Revoked state) and create a new `AppAccess` struct object
                    let mut app_access_vec: Vec<FfiAppAccess> = Vec::new();

                    for (user, perm_set) in permissions {
                        if let Key(public_key) = user {
                            let app_access = match apps.get(&public_key) {
                                Some(app_info) => {
                                    AppAccess {
                                        sign_key: public_key,
                                        permissions: perm_set,
                                        name: Some(app_info.name.clone()),
                                        app_id: Some(app_info.id.clone()),
                                    }
                                }
                                None => {
                                    // If an app is listed in the MD permissions list, but is not
                                    // listed in the registered apps list in Authenticator, then set
                                    // the app_id and app_name fields to ptr::null(), but provide
                                    // the public sign key and the list of permissions.
                                    AppAccess {
                                        sign_key: public_key,
                                        permissions: perm_set,
                                        name: None,
                                        app_id: None,
                                    }
                                }
                            };
                            app_access_vec.push(app_access.into_repr_c()?);
                        }
                    }

                    o_cb(
                        user_data.0,
                        FFI_RESULT_OK,
                        app_access_vec.as_safe_ptr(),
                        app_access_vec.len(),
                    );

                    Ok(())
                })
                .map_err(move |e| {
                    call_result_cb!(Err::<(), _>(e), user_data, o_cb);
                })
                .into_box()
                .into()
        })?;

        Ok(())
    })
}
