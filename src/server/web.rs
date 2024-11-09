use crate::{config, db::Connection, utils::ping};
use eyre::Result;
use html::get_index;
use rocket::{
    get, post,
    response::{content::RawHtml, Responder},
    routes,
    serde::{json::Json, Deserialize, Serialize},
};
use std::{collections::HashMap, env, str::FromStr};
use url::Url;

use super::{metrics::MountMetrics, DB, METRICS, NEW_CO_TX};

mod html;

#[derive(Serialize)]
struct ApiResponse {
    mollysocket: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct ConnectionData {
    pub uuid: String,
    pub device_id: u32,
    pub password: String,
    pub endpoint: String,
    pub ping: Option<bool>,
}

#[derive(Debug)]
enum RegistrationStatus {
    /// This is a new connection
    New,
    /// The credentials are updated
    CredsUpdated,
    /// The credentials are updated but the current connection is not forbidden
    IgnoreCredsUpdated,
    /// The credentials are the same, the endpoint is updated
    EndpointUpdated,
    /// The credentials and the endpoint are the same, and the connection in healthy
    Running,
    /// The credentials are the same, and the connection in forbidden
    Forbidden,
    /// The account id is forbidden
    InvalidUuid,
    /// The endpoint is forbidden
    InvalidEndpoint,
    /// Another error occurred
    InternalError,
}

// This is used to send the reponse to Molly
impl From<RegistrationStatus> for String {
    fn from(r: RegistrationStatus) -> Self {
        match r {
            RegistrationStatus::New
            | RegistrationStatus::CredsUpdated
            | RegistrationStatus::EndpointUpdated
            | RegistrationStatus::Running => "ok",
            RegistrationStatus::Forbidden => "forbidden",
            RegistrationStatus::InvalidUuid => "invalid_uuid",
            RegistrationStatus::InvalidEndpoint => "invalid_endpoint",
            // If someone tries to register new creds for an healthy connection,
            // we return an internal_error.
            RegistrationStatus::IgnoreCredsUpdated | RegistrationStatus::InternalError => {
                "internal_error"
            }
        }
        .into()
    }
}

struct Req<'r> {
    ua: &'r str,
    uri: Option<String>,
    airgapped: bool,
}

#[rocket::async_trait]
impl<'r> rocket::request::FromRequest<'r> for Req<'r> {
    type Error = ();

    async fn from_request(
        request: &'r rocket::request::Request<'_>,
    ) -> rocket::request::Outcome<Req<'r>, ()> {
        let ua = request.headers().get_one("user-agent").unwrap_or("");
        let airgapped = request.query_value::<&str>("airgapped").is_some();
        let origin = request
            .headers()
            .get_one("X-Original-URL")
            .map(|h| rocket::http::uri::Origin::parse(h).ok())
            .flatten()
            .unwrap_or_else(|| request.uri().clone());
        let path = origin.path().as_str();
        // We assume this is https
        let uri = request
            .host()
            .map(|h| format!("https://{}{}", h.to_string(), path));
        rocket::request::Outcome::Success(Req { ua, uri, airgapped })
    }
}

enum Resp {
    Json(Json<ApiResponse>),
    Html(RawHtml<String>),
}

impl<'r> Responder<'r, 'r> for Resp {
    fn respond_to(self, request: &'r rocket::Request<'_>) -> rocket::response::Result<'r> {
        match self {
            Resp::Json(r) => r.respond_to(request),
            Resp::Html(r) => r.respond_to(request),
        }
    }
}

#[get("/")]
fn index(req: Req) -> Resp {
    if req.ua.contains("Signal-Android") {
        Resp::Json(gen_api_rep(HashMap::new()))
    } else {
        Resp::Html(RawHtml(get_index(req.airgapped, req.uri.as_deref())))
    }
}

#[get("/discover")]
fn discover() -> Json<ApiResponse> {
    gen_api_rep(HashMap::new())
}

#[post("/", format = "application/json", data = "<co_data>")]
async fn register(co_data: Json<ConnectionData>) -> Json<ApiResponse> {
    let mut status = registration_status(&co_data).await;
    // Any error will be turned into internal_error
    match status {
        RegistrationStatus::New => handle_new_connection(&co_data, true, false).await,
        RegistrationStatus::CredsUpdated => handle_new_connection(&co_data, true, true).await,
        RegistrationStatus::EndpointUpdated => {
            handle_new_connection(&co_data, co_data.ping.unwrap_or(false), false).await
        }
        RegistrationStatus::Running => {
            // If the connection is "Running" then the device creds still exists,
            // if the user register on another server or delete the linked device,
            // then the connection ends with a 403 Forbidden
            // If the connection is for an invalid uuid or an error occured : we
            // have nothing to do, except if the request ask for a ping
            DB.update_last_registration(&co_data.uuid).unwrap();
            if co_data.ping.unwrap_or(false) {
                ping_endpoint(&co_data).await;
            }
            Ok(())
        }
        RegistrationStatus::IgnoreCredsUpdated
        | RegistrationStatus::Forbidden
        | RegistrationStatus::InvalidEndpoint
        | RegistrationStatus::InvalidUuid => Ok(()),
        _ => {
            log::debug!("Status unknown: {status:?}");
            Err(eyre::eyre!("Unknown status: {status:?}"))
        }
    }
    .unwrap_or_else(|_| status = RegistrationStatus::InternalError);

    log::debug!("Status: {status:?}");
    gen_api_rep(HashMap::from([(
        String::from("status"),
        String::from(status),
    )]))
}

/**
Add new a connection. Ping the endpoint if [ping],
decrease forbidden connections in metrics if
[dec_forbidden]
*/
async fn handle_new_connection(
    co_data: &Json<ConnectionData>,
    ping: bool,
    dec_forbidden: bool,
) -> Result<()> {
    if new_connection(&co_data).is_ok() {
        log::debug!("Connection successfully added.");
        if ping {
            ping_endpoint(&co_data).await;
        }
        if dec_forbidden {
            METRICS.forbiddens.dec();
        }
    } else {
        log::debug!("Could not start new connection");
    }
    Ok(())
}

fn new_connection(co_data: &Json<ConnectionData>) -> Result<()> {
    let co = Connection::new(
        co_data.uuid.clone(),
        co_data.device_id,
        co_data.password.clone(),
        co_data.endpoint.clone(),
    );
    DB.add(&co).unwrap();
    if let Some(tx) = &*NEW_CO_TX.lock().unwrap() {
        let _ = tx.unbounded_send(co);
    }
    Ok(())
}

async fn ping_endpoint(co_data: &ConnectionData) {
    if let Err(e) = ping(Url::from_str(&co_data.endpoint).unwrap()).await {
        log::warn!(
            "Cound not ping the connection (uuid={}): {e:?}",
            &co_data.uuid
        );
    }
}

async fn registration_status(co_data: &ConnectionData) -> RegistrationStatus {
    let endpoint_valid = config::is_endpoint_valid(&co_data.endpoint).await;
    let uuid_valid = config::is_uuid_valid(&co_data.uuid);

    if !uuid_valid {
        return RegistrationStatus::InvalidUuid;
    }

    if !endpoint_valid {
        return RegistrationStatus::InvalidEndpoint;
    }

    let co = match DB.get(&co_data.uuid) {
        Ok(co) => co,
        Err(_) => {
            return RegistrationStatus::New;
        }
    };

    if co.device_id == co_data.device_id && co.password == co_data.password {
        // Credentials are not updated
        if co.forbidden {
            RegistrationStatus::Forbidden
        } else if co.endpoint != co_data.endpoint {
            RegistrationStatus::EndpointUpdated
        } else {
            RegistrationStatus::Running
        }
    } else {
        // We return CredsUpdated only if the current connection is forbidden.
        // So it is impossible for someone to update a healthy connection
        // without the linked device password.
        if co.forbidden {
            RegistrationStatus::CredsUpdated
        } else {
            RegistrationStatus::IgnoreCredsUpdated
        }
    }
}

fn gen_api_rep(mut map: HashMap<String, String>) -> Json<ApiResponse> {
    map.insert(
        String::from("version"),
        env!("CARGO_PKG_VERSION").to_string(),
    );
    Json(ApiResponse { mollysocket: map })
}

pub async fn launch() {
    if !config::should_start_webserver() {
        log::warn!("The web server is disabled, making mollysocket run in an air gapped mode. With this clients are less easy to set up and push might break.");
        return;
    }

    let rocket_cfg = rocket::Config::figment()
        .merge(("address", config::get_host()))
        .merge(("port", config::get_port()));

    let _ = rocket::build()
        .configure(rocket_cfg)
        .mount("/", routes![index, discover, register])
        .mount_metrics("/metrics", &METRICS)
        .launch()
        .await;
}
