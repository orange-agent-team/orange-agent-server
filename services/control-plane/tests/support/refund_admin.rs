//! Independent development finance approval of an original-purchase refund.
pub(crate) use crate::finance_sessions::Admin;
use serde_json::{Value, json};
impl Admin {
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
