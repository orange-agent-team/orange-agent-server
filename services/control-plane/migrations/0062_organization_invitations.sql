CREATE TABLE organization_invitations(
 invitation_id text PRIMARY KEY,organization_id text NOT NULL REFERENCES organizations(organization_id),
 issuer_id text NOT NULL REFERENCES users(user_id),recipient_id text NOT NULL REFERENCES users(user_id),
 role text NOT NULL CHECK(role IN('owner','admin','member','viewer','billing')),
 request_digest text NOT NULL CHECK(request_digest ~ '^[a-f0-9]{64}$'),
 state text NOT NULL CHECK(state IN('pending','accepted','revoked','declined','expired')),
 expires_at timestamptz NOT NULL,accepted_version bigint,created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 CHECK(issuer_id<>recipient_id),CHECK((state='accepted')=(accepted_version IS NOT NULL))
);
CREATE UNIQUE INDEX one_pending_invite ON organization_invitations(organization_id,recipient_id) WHERE state='pending';
CREATE INDEX current_recipient_invites ON organization_invitations(recipient_id,created_at,invitation_id) WHERE state='pending';
CREATE FUNCTION protect_invitation_terms() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR OLD.state<>'pending' OR (to_jsonb(NEW)-'state'-'accepted_version') IS DISTINCT FROM (to_jsonb(OLD)-'state'-'accepted_version')
 THEN RAISE EXCEPTION 'immutable team invitation'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER protect_team_invitation BEFORE UPDATE OR DELETE ON organization_invitations FOR EACH ROW EXECUTE FUNCTION protect_invitation_terms();

CREATE FUNCTION revoke_departed_member_authority() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='UPDATE' AND NEW.role=OLD.role AND NEW.active=OLD.active THEN RETURN NEW; END IF;
 UPDATE organization_invitations SET state='revoked' WHERE organization_id=NEW.organization_id AND state='pending' AND (issuer_id=NEW.user_id OR recipient_id=NEW.user_id);
 IF NOT NEW.active OR NEW.role IN('billing','viewer') THEN
   UPDATE budget_user_grants g SET can_submit=false FROM budget_roots b WHERE g.root_budget_id=b.root_budget_id AND b.owner_id=NEW.organization_id AND g.user_id=NEW.user_id AND g.can_submit;
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER removed_member_authority AFTER INSERT OR UPDATE ON memberships FOR EACH ROW EXECUTE FUNCTION revoke_departed_member_authority();

CREATE FUNCTION revoke_restricted_invites() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_TABLE_NAME='owners' THEN
   IF NEW.state<>'active' THEN UPDATE organization_invitations SET state='revoked' WHERE organization_id=NEW.owner_id AND state='pending'; END IF;
 ELSIF NOT NEW.active THEN
   UPDATE organization_invitations SET state='revoked' WHERE state='pending' AND (issuer_id=NEW.user_id OR recipient_id=NEW.user_id);
 END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER restricted_org_invites AFTER UPDATE OF state ON owners FOR EACH ROW EXECUTE FUNCTION revoke_restricted_invites();
CREATE TRIGGER inactive_user_invites AFTER UPDATE OF active ON users FOR EACH ROW EXECUTE FUNCTION revoke_restricted_invites();
