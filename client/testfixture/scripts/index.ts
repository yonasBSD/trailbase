import { addRoute, parsePath, query, htmlHandler, jsonHandler, stringHandler } from "trailbase:main";
import type { JsonRequestType, ParsedPath, StringRequestType } from "../../../trailbase-core/js/src/index.ts";

addRoute("GET", "/test", stringHandler(async (req: StringRequestType) => {
  const uri : ParsedPath = parsePath(req.uri);

  const table = uri.query.get("table");
  if (table) {
    const rows = await query(`SELECT COUNT(*) FROM ${table}`, [])
    return `entries: ${rows[0][0]}`;
  }

  return `test: ${req.uri}`;
}));

addRoute("GET", "/test/:table", stringHandler(async (req: StringRequestType) => {
  const table = req.params["table"];
  if (table) {
    const rows = await query(`SELECT COUNT(*) FROM ${table}`, [])
    return `entries: ${rows[0][0]}`;
  }

  return `test: ${req.uri}`;
}));

addRoute("GET", "/html", htmlHandler((_req: StringRequestType) => {
  return `
    <html>
      <body>
        <h1>Html Handler</h1>
      </body>
    </html>
  `;
}));

addRoute("GET", "/json", jsonHandler((_req: JsonRequestType) => {
  return {
    int: 5,
    real: 4.2,
    msg: "foo",
    obj: {
      nested: true,
    }
  };
}));