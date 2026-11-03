//! おまかせ・書き出しの API（統合後に追加する）
use crate::server::AppState;
use axum::Router;
use std::sync::Arc;

pub fn routes(r: Router<Arc<AppState>>) -> Router<Arc<AppState>> {
    r
}
