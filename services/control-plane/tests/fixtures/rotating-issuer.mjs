// Isolated issuer for the signing-key rollover test. All keys die with the process.
import { createHash, generateKeyPairSync, randomUUID, sign } from "node:crypto";
import { createServer } from "node:http";

const clientId = "orange-key-rollover-test";
const redirect = "https://127.0.0.1:43122/api/v1/auth/callback";
const pairs = ["first", "second"].map(kid => {
  const { publicKey, privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
  return { kid, jwk: { ...publicKey.export({ format: "jwk" }), kid, alg: "RS256", use: "sig" }, privateKey };
});
const codes = new Map();
let exchanges = 0;
let reads = 0;
let origin;
const json = (res, status, body) => {
  res.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" });
  res.end(JSON.stringify(body));
};
const encode = data => Buffer.from(JSON.stringify(data)).toString("base64url");

const server = createServer(async (req, res) => {
  const url = new URL(req.url, origin);
  const args = url.searchParams;
  if (url.pathname === "/.well-known/openid-configuration") {
    return json(res, 200, { issuer: origin, authorization_endpoint: origin + "/authorize",
      token_endpoint: origin + "/token", jwks_uri: origin + "/keys", response_types_supported: ["code"],
      subject_types_supported: ["public"], id_token_signing_alg_values_supported: ["RS256"],
      token_endpoint_auth_methods_supported: ["client_secret_basic"] });
  }
  if (url.pathname === "/keys") {
    reads++;
    if (exchanges === 3) return json(res, 503, { error: "temporarily_unavailable" });
    return json(res, 200, { keys: [pairs[exchanges < 2 ? 0 : 1].jwk] });
  }
  if (url.pathname === "/counts") return json(res, 200, { exchanges, reads });
  if (url.pathname === "/authorize" && req.method === "GET" && args.get("client_id") === clientId &&
      args.get("redirect_uri") === redirect && args.get("code_challenge_method") === "S256" &&
      args.get("state") && args.get("nonce") && args.get("code_challenge")) {
    const code = randomUUID();
    codes.set(code, { challenge: args.get("code_challenge"), nonce: args.get("nonce") });
    const location = new URL(redirect);
    location.searchParams.set("code", code);
    location.searchParams.set("state", args.get("state"));
    res.writeHead(302, { location: location.href });
    return res.end();
  }
  if (url.pathname === "/token" && req.method === "POST") {
    let body = "";
    for await (const chunk of req) {
      body += chunk;
      if (body.length > 8192) return json(res, 400, {});
    }
    const form = new URLSearchParams(body);
    const code = form.get("code");
    const grant = codes.get(code);
    codes.delete(code);
    const challenge = createHash("sha256").update(form.get("code_verifier") ?? "").digest("base64url");
    if (!grant || grant.challenge !== challenge || form.get("redirect_uri") !== redirect) {
      return json(res, 400, { error: "invalid_grant" });
    }
    exchanges++;
    // Negative protocol vectors use only this disposable issuer's own credentials.
    const key = pairs[exchanges === 1 || exchanges === 5 ? 0 : 1];
    const now = Math.floor(Date.now() / 1000);
    const data = encode({ alg: "RS256", kid: key.kid }) + "." +
      encode({ iss: origin, aud: exchanges === 6 ? "another-fixture-client" : clientId, sub: "fixture-subject",
               nonce: exchanges === 7 ? "different-fixture-nonce" : grant.nonce, acr: "fixture",
               auth_time: now, iat: now, exp: exchanges === 8 ? now - 300 : now + 180 });
    const signature = sign("RSA-SHA256", Buffer.from(data), key.privateKey).toString("base64url");
    return json(res, 200, { token_type: "Bearer", access_token: randomUUID(), id_token: data + "." + signature });
  }
  json(res, 404, {});
});
server.listen(0, "127.0.0.1", () => {
  origin = "http://127.0.0.1:" + server.address().port;
  process.stdout.write(origin + "\n");
});
