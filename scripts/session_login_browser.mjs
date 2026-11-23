// Browser acceptance for the private local Keycloak realm and real user/Admin APIs.
import assert from "node:assert/strict";
import { createHmac } from "node:crypto";
import { readFileSync } from "node:fs";
import { createServer } from "node:https";
import { request } from "node:http";
import { createRequire } from "node:module";
import { join } from "node:path";

let input = "";
for await (const chunk of process.stdin) input += chunk;
const config = JSON.parse(input);
const { chromium } = createRequire(config.webPackage)("@playwright/test");
const identity = JSON.parse(readFileSync(config.fixture, "utf8")).users[0];
const issuer = "http://127.0.0.1:44180";
const loops = [["user", 43122, "/api/v1", "/organizations"], ["admin", 43112, "/admin/v1", "/overview"]];
const browser = await chromium.launch({ channel: "chrome", headless: true });
try {
  for (const [role, port, prefix, landing] of loops) {
    const site = "https://127.0.0.1:" + port;
    const target = new URL(config[role]);
    assert.equal(target.hostname, "127.0.0.1");
    assert.equal(target.protocol, "http:");
    const server = createServer({ key: readFileSync(join(config.tls, "key.pem")),
      cert: readFileSync(join(config.tls, "cert.pem")) }, (req, res) => {
      if (req.url === landing) { res.writeHead(200, { "content-type": "text/plain" }); res.end("OK"); return; }
      if (!req.url.startsWith(prefix + "/")) { res.writeHead(404); res.end(); return; }
      const api = request({ hostname: target.hostname, port: target.port, method: req.method,
        path: req.url, headers: req.headers, timeout: 10000 }, reply => {
        res.writeHead(reply.statusCode, reply.headers);
        reply.pipe(res);
      });
      api.on("error", () => { res.writeHead(502); res.end(); });
      api.on("timeout", () => api.destroy());
      req.pipe(api);
    });
    await new Promise((resolve, reject) => server.once("error", reject).listen(port, "127.0.0.1", resolve));
    const ctx = await browser.newContext({ baseURL: site, ignoreHTTPSErrors: true });
    try {
      const page = await ctx.newPage();
      await page.route("**/*", route => [site, issuer].includes(new URL(route.request().url()).origin)
        ? route.continue() : route.abort());
      await page.goto(site + prefix + "/auth/start");
      assert.equal(new URL(page.url()).origin, issuer);
      await page.locator("#username").fill(identity.username);
      await page.locator("#password").fill(identity.password);
      await page.locator("#kc-login").click();
      if (role === "admin") {
        await page.locator("#otp").waitFor();
        assert.equal((await ctx.request.get(prefix + "/session")).status(), 401);
        await new Promise(resolve => setTimeout(resolve, 30200 - Date.now() % 30000));
        const counter = Buffer.alloc(8);
        counter.writeBigUInt64BE(BigInt(Math.floor(Date.now() / 30000)));
        const mac = createHmac("sha1", Buffer.from(identity.otp)).update(counter).digest();
        const otp = ((mac.readUInt32BE(mac.at(-1) & 15) & 0x7fffffff) % 1000000).toString().padStart(6, "0");
        await page.locator("#otp").fill(otp);
        await page.locator("#kc-login").click();
      }
      await page.waitForURL(site + landing, { timeout: 16000 });
      const reply = await ctx.request.get(prefix + "/session");
      assert.equal(reply.status(), 200);
      const view = await reply.json();
      if (role === "admin") {
        assert.equal(view.mfa_verified, true);
        assert.equal(view.principal_id, identity.principal_id);
      } else assert.ok(view.user_id && view.user_id === view.personal_owner_id);
      const cookie = (await ctx.cookies()).find(c => c.name === "__Host-orange-" + role);
      assert.ok(cookie?.httpOnly && cookie.secure);
      assert.equal((await ctx.request.delete(prefix + "/session", { headers: {
        Origin: site, "X-CSRF-Token": view.csrf_token, "Idempotency-Key": "session-acceptance-logout"
      } })).status(), 204);
      assert.equal((await ctx.request.get(prefix + "/session")).status(), 401);
      console.log("PASS live Keycloak " + role + " login and logout");
    } finally {
      await ctx.close();
      await new Promise(resolve => server.close(resolve));
    }
  }
} finally { await browser.close(); }
