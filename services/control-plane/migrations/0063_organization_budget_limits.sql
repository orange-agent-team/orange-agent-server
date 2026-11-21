LOCK TABLE organizations,budget_roots IN SHARE ROW EXCLUSIVE MODE;
CREATE TABLE organization_budget_policies(
 organization_id text PRIMARY KEY REFERENCES organizations(organization_id),
 owner_ceiling bigint CHECK(owner_ceiling>=0),hard_limit bigint CHECK(hard_limit>=0),
 allocated numeric NOT NULL DEFAULT 0 CHECK(allocated>=0),version bigint NOT NULL DEFAULT 0 CHECK(version>=0),
 CHECK((owner_ceiling IS NULL)=(hard_limit IS NULL)),CHECK(hard_limit<=owner_ceiling AND allocated<=hard_limit)
);
CREATE FUNCTION org_root_allocation(s text,cap bigint,spent bigint,held bigint) RETURNS numeric
LANGUAGE sql IMMUTABLE STRICT AS $$ SELECT CASE WHEN s='active' THEN cap::numeric ELSE spent::numeric+held END $$;
INSERT INTO organization_budget_policies(organization_id,allocated)
 SELECT o.organization_id,COALESCE(sum(org_root_allocation(b.state,b.limit_amount,b.spent,b.reserved)),0)
 FROM organizations o LEFT JOIN budget_roots b ON b.owner_id=o.organization_id GROUP BY o.organization_id;
CREATE FUNCTION new_org_budget_policy() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN INSERT INTO organization_budget_policies(organization_id) VALUES(NEW.organization_id);RETURN NEW;END $$;
CREATE TRIGGER initialize_org_budget AFTER INSERT ON organizations FOR EACH ROW EXECUTE FUNCTION new_org_budget_policy();
CREATE FUNCTION account_org_root_allocation() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE delta numeric; owner_key text;
BEGIN
 IF TG_OP='DELETE' THEN
   IF OLD.spent<>0 OR OLD.reserved<>0 THEN RAISE EXCEPTION 'cannot remove accounted budget'; END IF;
   owner_key=OLD.owner_id;delta=-org_root_allocation(OLD.state,OLD.limit_amount,OLD.spent,OLD.reserved);
 ELSE
   owner_key=NEW.owner_id;delta=org_root_allocation(NEW.state,NEW.limit_amount,NEW.spent,NEW.reserved);
   IF TG_OP='UPDATE' THEN
     IF NEW.owner_id<>OLD.owner_id THEN RAISE EXCEPTION 'immutable budget owner'; END IF;
     delta=delta-org_root_allocation(OLD.state,OLD.limit_amount,OLD.spent,OLD.reserved);
   END IF;
 END IF;
 IF delta<>0 THEN
    UPDATE organization_budget_policies SET allocated=allocated+delta WHERE organization_id=owner_key AND (hard_limit IS NULL OR allocated+delta<=hard_limit);
    IF NOT FOUND AND EXISTS(SELECT 1 FROM organization_budget_policies WHERE organization_id=owner_key) THEN
       RAISE EXCEPTION USING ERRCODE='OA041',MESSAGE='organization hard budget exceeded';
    END IF;
 END IF;
 IF TG_OP='DELETE' THEN RETURN OLD; END IF;RETURN NEW;
END $$;
CREATE TRIGGER account_org_budget AFTER INSERT OR UPDATE OR DELETE ON budget_roots FOR EACH ROW EXECUTE FUNCTION account_org_root_allocation();
CREATE TABLE member_budget_allocations(
 root_budget_id text PRIMARY KEY REFERENCES budget_roots(root_budget_id),
 organization_id text NOT NULL REFERENCES organizations(organization_id),member_id text NOT NULL REFERENCES users(user_id),creator_id text NOT NULL REFERENCES users(user_id),
 request_digest text NOT NULL CHECK(request_digest ~ '^[a-f0-9]{64}$'),version bigint NOT NULL DEFAULT 1 CHECK(version>0)
);
CREATE INDEX member_allocation_lookup ON member_budget_allocations(organization_id,member_id,root_budget_id);
ALTER TABLE budget_roots ADD CONSTRAINT budget_owner_pair UNIQUE(root_budget_id,owner_id);
ALTER TABLE member_budget_allocations ADD FOREIGN KEY(root_budget_id,organization_id) REFERENCES budget_roots(root_budget_id,owner_id);
CREATE FUNCTION bump_member_budget_generation() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 UPDATE member_budget_allocations SET version=version+1 WHERE root_budget_id=NEW.root_budget_id AND member_id=NEW.user_id;
 RETURN NEW;
END $$;
CREATE TRIGGER member_grant_generation AFTER UPDATE OF can_submit ON budget_user_grants FOR EACH ROW
WHEN(NEW.can_submit IS DISTINCT FROM OLD.can_submit) EXECUTE FUNCTION bump_member_budget_generation();
CREATE FUNCTION protect_member_budget_origin() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
 IF TG_OP='DELETE' OR (to_jsonb(NEW)-'version') IS DISTINCT FROM (to_jsonb(OLD)-'version') OR NEW.version<=OLD.version THEN RAISE EXCEPTION 'immutable allocation origin'; END IF;
 RETURN NEW;
END $$;
CREATE TRIGGER immutable_member_budget_origin BEFORE UPDATE OR DELETE ON member_budget_allocations FOR EACH ROW EXECUTE FUNCTION protect_member_budget_origin();
