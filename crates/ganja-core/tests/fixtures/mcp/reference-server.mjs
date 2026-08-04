// A reference MCP server, built on the official `@modelcontextprotocol/sdk`
// rather than on the crate under test.
//
// The point of the whole fixture is that it is somebody else's implementation:
// a client and a server built from one library agree with each other whether or
// not either of them is right, so an rmcp server here would certify rmcp
// against itself. This is the same SDK upstream opencode uses (1.29.0), taken
// out of the reference checkout the golden differential already requires, and
// the low-level `Server` is used rather than `McpServer` so that the tool
// schemas travel as the literal JSON a server puts on the wire.
//
// The SDK is imported by absolute path because this file lives in ganja's tree
// and not inside the checkout's `node_modules`, so a bare specifier would
// resolve from here and find nothing. `GANJA_MCP_SDK_DIR` is what the test
// resolves and passes in.

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

// Written to stderr on purpose: the test asserts that a server's chatter is
// drained rather than left to fill a pipe and wedge the connection.
process.stderr.write("reference server starting\n")

const text = { type: "object", properties: { text: { type: "string" } }, required: ["text"] }
const nothing = { type: "object", properties: {} }

const server = new Server(
  { name: "reference", version: "0.0.0" },
  {
    capabilities: { tools: {} },
    instructions: "Echo things back.\nDo not read anything into them.",
  },
)

server.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: [
    { name: "echo", description: "Repeats what it is given.", inputSchema: text },
    { name: "explode", description: "Always fails.", inputSchema: nothing },
    { name: "structured", description: "Answers with structure only.", inputSchema: nothing },
    { name: "picture", description: "Answers with an image.", inputSchema: nothing },
    { name: "vanish", description: "Ends the process.", inputSchema: nothing },
    // A name that has to be sanitized, so the naming rule is exercised against
    // a real listing rather than only against a table in a unit test.
    { name: "odd.name", description: "Has a name a tool may not have.", inputSchema: nothing },
  ],
}))

server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const name = request.params.name
  const given = request.params.arguments ?? {}

  switch (name) {
    case "echo":
      return { content: [{ type: "text", text: `echo: ${given.text ?? ""}` }] }
    case "odd.name":
      return { content: [{ type: "text", text: "the odd one answered" }] }
    case "explode":
      return { isError: true, content: [{ type: "text", text: "the fixture refused" }] }
    case "structured":
      return { content: [], structuredContent: { answered: true, count: 2 } }
    case "picture":
      // Nine bytes of payload, so the size in the omission line is checkable.
      return {
        content: [
          { type: "text", text: "here it is" },
          { type: "image", data: "MTIzNDU2Nzg5", mimeType: "image/png" },
        ],
      }
    case "vanish":
      // Killed rather than returned from: the client is left holding a
      // connection that has gone away mid-session.
      process.exit(3)
      break
    default:
      return { isError: true, content: [{ type: "text", text: `no such tool: ${name}` }] }
  }
})

await server.connect(new StdioServerTransport())
