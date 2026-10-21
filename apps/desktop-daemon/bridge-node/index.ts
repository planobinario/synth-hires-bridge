import Database from "better-sqlite3"
import WebSocket from "ws"
import { exec, spawn } from "node:child_process"
import { promises as fs } from "node:fs"
import path from "node:path"
import os from "node:os"
import crypto from "node:crypto"

// --- Protocol Constants ---
const PROTOCOL_VERSION = 1

export interface BridgeCapability {
  capability: string
}

export class LocalBridgeDaemon {
  private db: Database.Database
  private ws: WebSocket | null = null
  private backendUrl: String
  private deviceToken: string
  private deviceId: string
  private deviceName: string
  private heartbeatInterval: NodeJS.Timeout | null = null

  constructor(options: {
    dbPath?: string
    backendUrl: string
    deviceToken: string
    deviceId: string
    deviceName?: string
  }) {
    const dbFile = options.dbPath || path.join(os.homedir(), ".synthhires", "bridge.db")
    const dir = path.dirname(dbFile)
    if (!require("fs").existsSync(dir)) {
      require("fs").mkdirSync(dir, { recursive: true })
    }

    this.db = new Database(dbFile)
    this.backendUrl = options.backendUrl
    this.deviceToken = options.deviceToken
    this.deviceId = options.deviceId
    this.deviceName = options.deviceName || `${os.hostname()} (Desktop Node Bridge)`

    this.initDatabase()
  }

  private initDatabase() {
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS local_conversations (
        id TEXT PRIMARY KEY,
        title TEXT,
        workspace_ref TEXT,
        model TEXT,
        provider TEXT,
        is_pinned INTEGER DEFAULT 0,
        updated_at INTEGER
      );

      CREATE TABLE IF NOT EXISTS local_messages (
        id TEXT PRIMARY KEY,
        conversation_id TEXT,
        role TEXT,
        content TEXT,
        created_at INTEGER,
        FOREIGN KEY(conversation_id) REFERENCES local_conversations(id) ON DELETE CASCADE
      );

      CREATE TABLE IF NOT EXISTS local_artifacts (
        id TEXT PRIMARY KEY,
        conversation_id TEXT,
        title TEXT,
        kind TEXT,
        content TEXT DEFAULT '',
        updated_at INTEGER
      );
      CREATE TABLE IF NOT EXISTS local_projects (
        id TEXT PRIMARY KEY,
        name TEXT,
        description TEXT,
        updated_at INTEGER
      );

      CREATE TABLE IF NOT EXISTS action_audit_logs (
        id TEXT PRIMARY KEY,
        capability TEXT,
        params TEXT,
        status TEXT,
        stdout TEXT,
        stderr TEXT,
        executed_at INTEGER
      );
    `)
  }

  public saveConversation(conv: {
    id: string
    title?: string
    workspaceRef?: any
    model?: string
    provider?: string
    isPinned?: boolean
    updatedAt?: number
  }) {
    const stmt = this.db.prepare(`
      INSERT INTO local_conversations (id, title, workspace_ref, model, provider, is_pinned, updated_at)
      VALUES (?, ?, ?, ?, ?, ?, ?)
      ON CONFLICT(id) DO UPDATE SET
        title = excluded.title,
        workspace_ref = excluded.workspace_ref,
        model = excluded.model,
        provider = excluded.provider,
        is_pinned = excluded.is_pinned,
        updated_at = excluded.updated_at
    `)
    stmt.run(
      conv.id,
      conv.title || "New conversation",
      conv.workspaceRef ? JSON.stringify(conv.workspaceRef) : null,
      conv.model || "gpt-4o",
      conv.provider || "openai",
      conv.isPinned ? 1 : 0,
      conv.updatedAt || Date.now()
    )
  }

  public saveMessage(msg: {
    id?: string
    conversationId: string
    role: string
    content: string
    createdAt?: number
  }) {
    const id = msg.id || crypto.randomUUID()
    const stmt = this.db.prepare(`
      INSERT INTO local_messages (id, conversation_id, role, content, created_at)
      VALUES (?, ?, ?, ?, ?)
      ON CONFLICT(id) DO NOTHING
    `)
    stmt.run(id, msg.conversationId, msg.role, msg.content, msg.createdAt || Date.now())
  }

  public getMessages(conversationId: string) {
    const stmt = this.db.prepare(`
      SELECT * FROM local_messages WHERE conversation_id = ? ORDER BY created_at ASC
    `)
    return stmt.all(conversationId)
  }

  public logAction(capability: string, params: any, status: string, stdout = "", stderr = "") {
    const stmt = this.db.prepare(`
      INSERT INTO action_audit_logs (id, capability, params, status, stdout, stderr, executed_at)
      VALUES (?, ?, ?, ?, ?, ?, ?)
    `)
    stmt.run(
      crypto.randomUUID(),
      capability,
      JSON.stringify(params),
      status,
      stdout,
      stderr,
      Date.now()
    )
  }

  public startWebSocket() {
    const wsUrl = this.backendUrl.replace(/^http/, "ws") + "/api/devices/ws"
    const tokenHash = crypto.createHash("sha256").update(this.deviceToken).digest("hex")

    console.log(`[bridge-daemon] Connecting to ${wsUrl} for device ${this.deviceId}...`)

    this.ws = new WebSocket(wsUrl, {
      headers: {
        "Sec-WebSocket-Protocol": `bearer.${this.deviceToken}`,
        "x-bridge-token-hash": tokenHash,
      },
    })

    this.ws.on("open", () => {
      console.log("[bridge-daemon] WebSocket connected. Sending hello frame...")
      this.sendFrame({
        v: PROTOCOL_VERSION,
        kind: "hello",
        tokenHash,
        fingerprint: `node-${os.arch()}-${os.platform()}`,
        deviceKind: "desktop",
        deviceName: this.deviceName,
        clientVersion: "1.0.0",
      })

      // Start heartbeat every 30s
      this.heartbeatInterval = setInterval(() => {
        this.sendFrame({
          v: PROTOCOL_VERSION,
          kind: "heartbeat",
          t: Date.now(),
        })
      }, 30000)
    })

    this.ws.on("message", async (data: Buffer | string) => {
      try {
        const frame = JSON.parse(data.toString())
        await this.handleFrame(frame)
      } catch (err: any) {
        console.error("[bridge-daemon] Error handling message:", err.message)
      }
    })

    this.ws.on("close", (code, reason) => {
      console.warn(`[bridge-daemon] WebSocket closed: ${code} - ${reason.toString()}`)
      if (this.heartbeatInterval) clearInterval(this.heartbeatInterval)
      // Reconnect after 5 seconds
      setTimeout(() => this.startWebSocket(), 5000)
    })

    this.ws.on("error", (err) => {
      console.error("[bridge-daemon] WebSocket error:", err.message)
    })
  }

  private sendFrame(frame: any) {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(frame))
    }
  }

  private async handleFrame(frame: any) {
    switch (frame.kind) {
      case "hello_ack":
        console.log(`[bridge-daemon] Hello ACK received! Device ID: ${frame.deviceId}`)
        break
      case "heartbeat_ack":
        // Heartbeat acknowledged
        break
      case "action_request":
        await this.executeAction(frame)
        break
      default:
        console.log(`[bridge-daemon] Frame received: ${frame.kind}`)
    }
  }

  private async executeAction(frame: { id: string; capability: string; params: any }) {
    const { id, capability, params } = frame
    console.log(`[bridge-daemon] Executing capability: ${capability}`, params)

    try {
      if (capability === "desktop.shell.execute") {
        const cmd = params.command || params.cmd
        exec(cmd, { cwd: params.cwd || os.homedir() }, (error, stdout, stderr) => {
          this.logAction(capability, params, error ? "error" : "success", stdout, stderr)
          this.sendFrame({
            v: PROTOCOL_VERSION,
            kind: "action_result",
            id,
            success: !error,
            stdout,
            stderr,
            exitCode: error ? error.code || 1 : 0,
          })
        })
      } else if (capability === "desktop.fs.read") {
        const content = await fs.readFile(params.path, "utf-8")
        this.logAction(capability, params, "success")
        this.sendFrame({
          v: PROTOCOL_VERSION,
          kind: "action_result",
          id,
          success: true,
          content,
        })
      } else if (capability === "desktop.fs.write") {
        await fs.mkdir(path.dirname(params.path), { recursive: true })
        await fs.writeFile(params.path, params.content, "utf-8")
        this.logAction(capability, params, "success")
        this.sendFrame({
          v: PROTOCOL_VERSION,
          kind: "action_result",
          id,
          success: true,
        })
      } else if (capability === "sync.save_chat") {
        if (params.conversation) this.saveConversation(params.conversation)
        if (params.messages && Array.isArray(params.messages)) {
          for (const m of params.messages) this.saveMessage(m)
        }
        this.sendFrame({
          v: PROTOCOL_VERSION,
          kind: "action_result",
          id,
          success: true,
        })
      } else {
        this.sendFrame({
          v: PROTOCOL_VERSION,
          kind: "action_result",
          id,
          success: false,
          error: `Capability not implemented in bridge: ${capability}`,
        })
      }
    } catch (err: any) {
      this.logAction(capability, params, "error", "", err.message)
      this.sendFrame({
        v: PROTOCOL_VERSION,
        kind: "action_result",
        id,
        success: false,
        error: err.message,
      })
    }
  }
}

// CLI runner entrypoint if executed directly
if (require.main === module) {
  // ─────────────────────────────────────────────────────────────────────────
  // REFERENCE IMPLEMENTATION — NOT A SHIPPED DAEMON.
  //
  // This file is a PROTOTYPE kept for protocol reference. It has NO
  // capability gate, NO path checks and NO consent flow: `desktop.shell
  // .execute` runs `exec(command)` and `desktop.fs.read`/`write` touch
  // whatever path the frame asks for. The Rust daemon
  // (crates/daemon-core) is the one that implements the real boundary —
  // CapabilityGate, path_gate with symlink-escape checks, the consent
  // broker and the encrypted audit log.
  //
  // Running this against a real backend would hand the account a device
  // with full, unprompted access to the machine. It therefore REFUSES to
  // start unless the operator opts in explicitly.
  // ─────────────────────────────────────────────────────────────────────────
  if (process.env.SYNTHHIRES_ALLOW_UNSAFE_REFERENCE_DAEMON !== "1") {
    console.error(
      "bridge-node is a REFERENCE prototype with no capability gate, no path " +
        "checks and no consent flow.\n" +
        "It refuses to start. Use the Rust daemon (crates/daemon-core).\n" +
        "If you really know what you are doing, set " +
        "SYNTHHIRES_ALLOW_UNSAFE_REFERENCE_DAEMON=1.",
    )
    process.exit(1)
  }
  console.warn(
    "[bridge-node] WARNING: running the UNSAFE reference daemon — no capability " +
      "gate, no path checks, no consent.",
  )

  const backendUrl = process.env.SYNTHHIRES_BACKEND_URL || "https://synthhires.com"
  const deviceToken = process.env.SYNTHHIRES_DEVICE_TOKEN || "demo-token"
  const deviceId = process.env.SYNTHHIRES_DEVICE_ID || "dev-" + crypto.randomUUID().slice(0, 8)

  const daemon = new LocalBridgeDaemon({
    backendUrl,
    deviceToken,
    deviceId,
  })

  daemon.startWebSocket()
}
