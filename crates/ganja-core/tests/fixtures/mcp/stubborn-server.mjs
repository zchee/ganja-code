// A reference MCP server that does NOT die when its stdin closes.
//
// It exists to make one test non-vacuous. Closing the child's stdin is what
// normally ends a stdio MCP server, and `reference-server.mjs` obliges — which
// means a test using it would pass whether or not the client had any other way
// to end a child, and would therefore pin nothing.
//
// This one keeps a timer on the event loop forever, so stdin EOF leaves it
// running. The only thing that can then end it is the client killing the
// process, which is exactly the guarantee under test.
//
// Built on the same `@modelcontextprotocol/sdk` as its sibling and for the same
// reason: a fixture that spoke a hand-rolled protocol would not survive the
// handshake, and the server has to connect before there is a child worth
// orphaning.

import { pathToFileURL } from "node:url"

const sdk = process.env.GANJA_MCP_SDK_DIR
if (!sdk) {
  process.stderr.write("GANJA_MCP_SDK_DIR is not set\n")
  process.exit(2)
}

const at = (relative) => import(pathToFileURL(`${sdk}/dist/esm/${relative}`).href)

const { Server } = await at("server/index.js")
const { StdioServerTransport } = await at("server/stdio.js")
const { ListToolsRequestSchema } = await at("types.js")

const server = new Server(
  { name: "stubborn", version: "0.0.0" },
  { capabilities: { tools: {} } },
)

server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: [
    {
      name: "linger",
      description: "Does nothing, slowly.",
      inputSchema: { type: "object", properties: {} },
    },
  ],
}))

await server.connect(new StdioServerTransport())

// The whole point. Without this the process would exit as soon as stdin closed
// and the test above it would prove nothing. Unref'ing it would defeat the
// purpose, so it is deliberately left holding the loop open.
setInterval(() => {}, 1_000)
