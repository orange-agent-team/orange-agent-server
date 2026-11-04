use super::*;
use crate::node_journal::ReceivedNodeIntent;
use orange_protocol::node_control::StopKind;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedNodePolicy {
    pub schema_version: u32,
    pub command_id: String,
    pub node_id: String,
    pub policy_generation: String,
    pub payload_digest: String,
    pub source_receipt_id: String,
    pub application_id: String,
    pub broker_epoch: String,
    pub kind: StopKind,
}
#[derive(Clone, Debug, Serialize)]
pub struct NodePolicyView {
    pub enrolled_node_id: Option<String>,
    pub policy_generation: String,
    pub platform_blocked: bool,
}
impl Broker {
    /// Trusted enrollment/bootstrap hook; no remote endpoint exposes this method.
    /// Rebinding an existing Broker to another node requires a separate, explicit
    /// enrollment migration with cleanup evidence, never a config overwrite.
    pub fn bind_enrolled_node(&mut self, node: &str) -> Result<(), BrokerError> {
        if !valid_id(node)
            || !node
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
        {
            return Err(BrokerError::Invalid);
        }
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<String> = tx.query_row(
            "SELECT enrolled_node_id FROM broker_meta WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        if existing.as_deref().is_some_and(|id| id != node) {
            return Err(BrokerError::Conflict);
        }
        if existing.is_none() {
            let historical = {
                let mut statement =
                    tx.prepare("SELECT DISTINCT remote_binding FROM grants WHERE mode='remote'")?;
                statement
                    .query_map([], |r| r.get::<_, Option<String>>(0))?
                    .collect::<Result<Vec<_>, _>>()?
            };
            for binding in historical {
                let binding: RemoteBinding =
                    decode(binding.as_deref().ok_or(BrokerError::RecoveryRequired)?)?;
                if binding.node_id != node {
                    return Err(BrokerError::Conflict);
                }
            }
            tx.execute(
                "UPDATE broker_meta SET enrolled_node_id=? WHERE singleton=1",
                [node],
            )?;
            tx.execute(
                "INSERT INTO broker_events(kind,details) VALUES('node.enrollment_bound',?)",
                [node],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn node_policy(&self) -> Result<NodePolicyView, BrokerError> {
        let (node,generation,blocked):(Option<String>,i64,bool)=self.connection.query_row("SELECT enrolled_node_id,node_policy_generation,platform_blocked FROM broker_meta WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        Ok(NodePolicyView {
            enrolled_node_id: node,
            policy_generation: generation.to_string(),
            platform_blocked: blocked,
        })
    }
    pub fn applied_node_policy(
        &self,
        command: &str,
    ) -> Result<Option<AppliedNodePolicy>, BrokerError> {
        if !valid_id(command) {
            return Err(BrokerError::Invalid);
        }
        let result: Option<String> = self
            .connection
            .query_row(
                "SELECT result FROM node_policy_commands WHERE command_id=?",
                [command],
                |r| r.get(0),
            )
            .optional()?;
        result
            .map(|raw| decode::<AppliedNodePolicy>(&raw))
            .transpose()
    }
    /// Called only by the current, policy-capable mTLS adapter after consulting
    /// the local durable receipt and the latest server policy generation.
    pub(crate) fn apply_verified_node_stop(
        &mut self,
        received: &ReceivedNodeIntent,
        deadline: Instant,
    ) -> Result<AppliedNodePolicy, BrokerError> {
        if Instant::now() >= deadline {
            return Err(BrokerError::Stale);
        }
        let intent = &received.intent;
        if !intent.validate()
            || !valid_id(&received.receipt_id)
            || intent.digest().map_err(|_| BrokerError::Invalid)? != received.payload_digest
        {
            return Err(BrokerError::Invalid);
        }
        let generation = intent.generation().ok_or(BrokerError::Invalid)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (node, current,blocked): (Option<String>, i64,bool) = tx.query_row(
            "SELECT enrolled_node_id,node_policy_generation,platform_blocked FROM broker_meta WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?,r.get(2)?)),
        )?;
        if node.as_deref() != Some(intent.node_id.as_str()) {
            return Err(BrokerError::Unauthorized);
        }
        if generation < current {
            return Err(BrokerError::Stale);
        }
        let previous:Option<(String,String,String)>=tx.query_row("SELECT payload_digest,source_receipt_id,result FROM node_policy_commands WHERE command_id=?",[&intent.command_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((digest, receipt, result)) = previous {
            if digest != received.payload_digest || receipt != received.receipt_id {
                return Err(BrokerError::Conflict);
            }
            if current != generation || !blocked || Instant::now() >= deadline {
                return Err(BrokerError::RecoveryRequired);
            }
            return decode(&result);
        }
        if generation <= current {
            return Err(BrokerError::Conflict);
        }
        let grants = {
            let mut statement=tx.prepare("SELECT g.device_id,g.grant_id FROM grants g JOIN slots s ON s.grant_id=g.grant_id WHERE g.mode='remote' AND g.epoch=? AND g.state IN('reserved','running') AND s.state IN('reserved_remote','running_remote')")?;
            statement
                .query_map([self.epoch], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (device, grant) in grants {
            tx.execute(
                "UPDATE grants SET state='draining' WHERE grant_id=?",
                [&grant],
            )?;
            tx.execute("UPDATE slots SET state='draining',reason='platform_node_stop' WHERE device_id=? AND grant_id=?",params![device,grant])?;
            tx.execute("INSERT INTO broker_events(device_id,grant_id,kind,details) VALUES(?,?,'grant.draining',?)",params![device,grant,intent.command_id])?;
        }
        tx.execute("UPDATE broker_meta SET remote_allowed=0,platform_blocked=1,node_policy_generation=? WHERE singleton=1",[generation])?;
        let result = AppliedNodePolicy {
            schema_version: 1,
            command_id: intent.command_id.clone(),
            node_id: intent.node_id.clone(),
            policy_generation: intent.policy_generation.clone(),
            payload_digest: received.payload_digest.clone(),
            source_receipt_id: received.receipt_id.clone(),
            application_id: uuid::Uuid::new_v4().to_string(),
            broker_epoch: self.epoch.to_string(),
            kind: intent.kind,
        };
        tx.execute("INSERT INTO node_policy_commands(command_id,node_id,generation,payload_digest,source_receipt_id,result) VALUES(?,?,?,?,?,?)",params![intent.command_id,intent.node_id,generation,received.payload_digest,received.receipt_id,json(&result)?])?;
        tx.execute(
            "INSERT INTO broker_events(kind,details) VALUES('node.policy_applied',?)",
            [json(&result)?],
        )?;
        if Instant::now() >= deadline {
            return Err(BrokerError::Stale);
        }
        tx.commit()?;
        Ok(result)
    }
}
