// A reference MCP server whose tool set moves, and which says so.
//
// Same SDK and the same reasoning as `reference-server.mjs`: the notification
// under test is a message on the wire, and a fixture built on the crate that
// receives it would agree with the receiver whether or not either of them read
// the specification correctly. This one is the official implementation, so the
// notification it sends is the one a real server sends.
//
// The change is deliberately in **both directions** — one tool appears and
// another disappears in the same `tools/list` — because a client that merely
// accumulated whatever it was told about would satisfy an add-only fixture
// without ever having replaced anything.

import { pathToFileURL } from "node:url"

const sdk = process.env.GANJA_MCP_SDK_DIR
if (!sdk) {
  process.stderr.write("GANJA_MCP_SDK_DIR is not set\n")
  process.exit(2)
}

const at = (relative) => import(pathToFileURL(`${sdk}/dist/esm/${relative}`).href)

const { Server } = await at("server/index.js")
const { StdioServerTransport } = await at("server/stdio.js")
const { CallToolRequestSchema, ListToolsRequestSchema } = await at("types.js")

const nothing = { type: "object", properties: {} }

const change = { name: "change", description: "Changes the tool set.", inputSchema: nothing }
const withdrawn = { name: "withdrawn", description: "Is listed until it is not.", inputSchema: nothing }
const added = { name: "added", description: "Is not listed until it is.", inputSchema: nothing }

// What the next `tools/list` answers. `change` survives both listings so that
// the tool driving this is never the tool being watched.
let listing = [change, withdrawn]

const server = new Server(
  { name: "changing", version: "0.0.0" },
  // `listChanged` is the capability that permits the notification at all: the
  // SDK refuses to send one a server never said it could.
  { capabilities: { tools: { listChanged: true } } },
)

server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: listing }))

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  switch (request.params.name) {
    case "change":
      listing = [change, added]
      // Sent before the result, so a client that re-lists on the notification
      // is already looking at the new set by the time the call it was made
      // from has finished.
      await server.sendToolListChanged()
      return { content: [{ type: "text", text: "the tool set moved" }] }
    // Neither of these is called by the test, and both are answered anyway: a
    // server that lists a tool it will not run is lying about its own surface,
    // and a fixture that lied could mask a client bug rather than expose one.
    // What is listed is callable.
    case "withdrawn":
      return { content: [{ type: "text", text: "still here" }] }
    case "added":
      return { content: [{ type: "text", text: "here now" }] }
    default:
      return { isError: true, content: [{ type: "text", text: `no such tool: ${request.params.name}` }] }
  }
})

await server.connect(new StdioServerTransport())
