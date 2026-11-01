// A fake OpenAI chat completions endpoint for the demo: no real model, no real spend. It answers
// after a moment with token counts the size of a real agent's call.
import http from "node:http";

const port = Number(process.env.MOCK_PORT || 19080);
const between = (lo, hi) => lo + Math.floor(Math.random() * (hi - lo));

http
  .createServer((req, res) => {
    req.resume();
    req.on("end", () => {
      const prompt_tokens = between(18000, 26000);
      const completion_tokens = between(1500, 2600);
      const reply = JSON.stringify({
        id: "chatcmpl-demo",
        object: "chat.completion",
        created: 0,
        model: "gpt-4o",
        choices: [{ index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" }],
        usage: { prompt_tokens, completion_tokens, total_tokens: prompt_tokens + completion_tokens },
      });
      setTimeout(() => {
        res.writeHead(200, { "content-type": "application/json", "content-length": Buffer.byteLength(reply) });
        res.end(reply);
      }, between(120, 260));
    });
  })
  .listen(port, "127.0.0.1");
