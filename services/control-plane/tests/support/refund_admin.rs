//! Distinct development finance identities exercising the real scoped Admin HTTP API.
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use orange_control_plane::store::Store;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
pub(crate) struct Operator {
    pub(crate) id: String,
    pub(crate) token: String,
    pub(crate) csrf: String,
}
async fn operator(pool: &sqlx::PgPool, issuer: &str, owner: &str) -> Operator {
    use orange_control_plane::admin::{hash_secret, random_secret};
    let a = Operator {
        id: Uuid::new_v4().to_string(),
        token: random_secret(),
        csrf: random_secret(),
    };
    sqlx::query("INSERT INTO admin_principals(principal_id,issuer,subject) VALUES($1,$2,$1)")
        .bind(&a.id)
        .bind(issuer)
        .execute(pool)
        .await
        .expect("finance principal");
    sqlx::query("INSERT INTO admin_grants(principal_id,role,environment,scope_kind,scope_id,expires_at) VALUES($1,'finance_operator','dev','tenant',$2,floor(extract(epoch FROM clock_timestamp()))::bigint+3600)")
       .bind(&a.id).bind(owner).execute(pool).await.expect("finance grant");
    sqlx::query("INSERT INTO admin_sessions(token_hash,principal_id,csrf_token,auth_time,idle_expires_at,absolute_expires_at) VALUES($1,$2,$3,floor(extract(epoch FROM clock_timestamp()))::bigint,floor(extract(epoch FROM clock_timestamp()))::bigint+600,floor(extract(epoch FROM clock_timestamp()))::bigint+3600)")
      .bind(hash_secret(&a.token)).bind(&a.id).bind(&a.csrf).execute(pool).await.expect("explicit independent session");
    a
}
pub(crate) struct Admin {
    pub(crate) app: Router,
    pub(crate) origin: String,
    pub(crate) maker: Operator,
    pub(crate) checker: Operator,
}
impl Admin {
    pub(crate) async fn new(db: &Store, owner: &str) -> Self {
        use orange_control_plane::admin::{
            identity::{AdminIdentity, OidcConfig},
            routes,
        };
        let cfg: OidcConfig = serde_json::from_slice(
            &std::fs::read(
                std::env::var("ORANGE_TEST_OIDC_CONFIG_FILE").expect("admin identity path"),
            )
            .expect("config"),
        )
        .expect("OIDC");
        let origin = cfg.admin_origin.clone();
        let maker = operator(db.pool(), &cfg.issuer, owner).await;
        let checker = operator(db.pool(), &cfg.issuer, owner).await;
        let app = routes::router(
            AdminIdentity::discover(db.pool().clone(), cfg)
                .await
                .expect("local admin OIDC"),
        );
        Self {
            app,
            origin,
            maker,
            checker,
        }
    }
    async fn call(
        &self,
        who: &Operator,
        method: &str,
        path: &str,
        etag: &str,
        body: Value,
        status: u16,
    ) -> (Value, String) {
        let request = Request::builder()
            .uri(path)
            .method(method)
            .header("cookie", format!("__Host-orange-admin={}", who.token))
            .header("origin", &self.origin)
            .header("x-csrf-token", &who.csrf)
            .header("idempotency-key", Uuid::new_v4().to_string())
            .header("if-match", etag)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .expect("Admin request");
        let res = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("Admin router");
        let code = res.status().as_u16();
        let etag = res
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();
        let v: Value =
            serde_json::from_slice(&to_bytes(res.into_body(), 65536).await.expect("bounded"))
                .expect("response JSON");
        assert_eq!(code, status, "{path}: {v}");
        (v, etag)
    }
    pub(crate) async fn approve(&self, owner: &str, refund: &Value) {
        let case = refund["case_id"].as_str().expect("case");
        let (r,_)=self.call(&self.maker,"GET",&format!("/admin/v1/test-refunds/{case}?environment=dev&scope_kind=tenant&scope_id={owner}"),"",Value::Null,200).await;
        assert_eq!(r["held_microcredits"], refund["held_microcredits"]);
        let req = json!({"scope":{"environment":"dev","kind":"tenant","scope_id":owner},"target_version":r["target_version"],"ticket_id":"refund-review","reason":"Review original test purchase and refundable units","action_type":"payment.refund",
           "payload":{"payment_order_id":r["payment_order_id"],"amount":r["amount"],"reconciliation_case_id":case}});
        let mut wrong = req.clone();
        wrong["payload"]["amount"]["minor_units"] = json!("500");
        self.call(&self.maker, "POST", "/admin/v1/actions", "", wrong, 412)
            .await;
        let (action, tag) = self
            .call(&self.maker, "POST", "/admin/v1/actions", "", req, 201)
            .await;
        let id = action["action_id"].as_str().expect("action");
        let approval = json!({"decision":"approve","reason":"Independently verified the scoped order","payload_digest":action["payload_digest"],"policy_version":action["policy_version"]});
        let path = format!("/admin/v1/actions/{id}/approvals");
        self.call(&self.maker, "POST", &path, &tag, approval.clone(), 403)
            .await;
        let (_, new_tag) = self
            .call(&self.checker, "POST", &path, &tag, approval, 200)
            .await;
        let execute = json!({"payload_digest":action["payload_digest"],"policy_version":action["policy_version"]});
        let (accepted, _) = self
            .call(
                &self.maker,
                "POST",
                &format!("/admin/v1/actions/{id}/execute"),
                &new_tag,
                execute,
                202,
            )
            .await;
        assert_eq!(accepted["business_ref"], refund["refund_id"]);
    }
}
