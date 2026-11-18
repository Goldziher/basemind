const express = require("express");

const app = express();
const router = express.Router();
const admin = express.Router();

function listUsers(req, res) {
  res.json([]);
}

const users = { show(req, res) {}, update(req, res) {} };

app.get("/health", (req, res) => res.send("ok"));
router.get("/users", listUsers);
router.get("/users/:id(\\d+)", auth, users.show);
router.put("/users/:id", users.update);
admin.delete("/cache/:key?", clearCache);
app.all("/ping", ping);

app.use("/api", router);
router.use("/admin", admin);

// Not routes: a settings read, a map lookup and two requests.
app.get("title");
cache.get(key, fallback);
axios.post("/users", user);
api.get("/users", { params: { page: 2 } });
