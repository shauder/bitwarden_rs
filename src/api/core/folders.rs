use rocket::State;
use rocket_contrib::{Json, Value};

use db::DbConn;
use db::models::*;

use api::{JsonResult, EmptyResult, JsonUpcase, WebSocketUsers, UpdateType};
use auth::Headers;

#[get("/folders")]
fn get_folders(headers: Headers, conn: DbConn) -> JsonResult {
    let folders = Folder::find_by_user(&headers.user.uuid, &conn);

    let folders_json: Vec<Value> = folders.iter().map(|c| c.to_json()).collect();

    Ok(Json(json!({
      "Data": folders_json,
      "Object": "list",
    })))
}

#[get("/folders/<uuid>")]
fn get_folder(uuid: String, headers: Headers, conn: DbConn) -> JsonResult {
    let folder = match Folder::find_by_uuid(&uuid, &conn) {
        Some(folder) => folder,
        _ => err!("Invalid folder")
    };

    if folder.user_uuid != headers.user.uuid {
        err!("Folder belongs to another user")
    }

    Ok(Json(folder.to_json()))
}

#[derive(Deserialize)]
#[allow(non_snake_case)]

pub struct FolderData {
    pub Name: String
}

#[post("/folders", data = "<data>")]
fn post_folders(data: JsonUpcase<FolderData>, headers: Headers, conn: DbConn, ws: State<WebSocketUsers>) -> JsonResult {
    let data: FolderData = data.into_inner().data;

    let mut folder = Folder::new(headers.user.uuid.clone(), data.Name);

    folder.save(&conn);
    ws.send_folder_update(UpdateType::SyncFolderCreate, &folder);

    Ok(Json(folder.to_json()))
}

#[post("/folders/<uuid>", data = "<data>")]
fn post_folder(uuid: String, data: JsonUpcase<FolderData>, headers: Headers, conn: DbConn, ws: State<WebSocketUsers>) -> JsonResult {
    put_folder(uuid, data, headers, conn, ws)
}

#[put("/folders/<uuid>", data = "<data>")]
fn put_folder(uuid: String, data: JsonUpcase<FolderData>, headers: Headers, conn: DbConn, ws: State<WebSocketUsers>) -> JsonResult {
    let data: FolderData = data.into_inner().data;

    let mut folder = match Folder::find_by_uuid(&uuid, &conn) {
        Some(folder) => folder,
        _ => err!("Invalid folder")
    };

    if folder.user_uuid != headers.user.uuid {
        err!("Folder belongs to another user")
    }

    folder.name = data.Name;

    folder.save(&conn);
    ws.send_folder_update(UpdateType::SyncFolderUpdate, &folder);

    Ok(Json(folder.to_json()))
}

#[post("/folders/<uuid>/delete")]
fn delete_folder_post(uuid: String, headers: Headers, conn: DbConn, ws: State<WebSocketUsers>) -> EmptyResult {
    delete_folder(uuid, headers, conn, ws)
}

#[delete("/folders/<uuid>")]
fn delete_folder(uuid: String, headers: Headers, conn: DbConn, ws: State<WebSocketUsers>) -> EmptyResult {
    let folder = match Folder::find_by_uuid(&uuid, &conn) {
        Some(folder) => folder,
        _ => err!("Invalid folder")
    };

    if folder.user_uuid != headers.user.uuid {
        err!("Folder belongs to another user")
    }

    // Delete the actual folder entry
    match folder.delete(&conn) {
        Ok(()) => {
            ws.send_folder_update(UpdateType::SyncFolderDelete, &folder);
            Ok(())
        }
        Err(_) => err!("Failed deleting folder")
    }
}
