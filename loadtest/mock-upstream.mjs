// A fake OpenAI chat completions endpoint for load tests: answers at once (or after
// MOCK_DELAY_MS) with a fixed reply and token counts, so only Tokenrein's own time is measured.
import http from "node:http";

const port = Number(process.env.MOCK_PORT || 18080);
const delay = Number(process.env.MOCK_DELAY_MS || 0);
const reply = JSON.stringify({
  id: "chatcmpl-loadtest",
  object: "chat.completion",
  created: 0,
  model: "gpt-4o-mini",
  choices: [{ index: 0, message: { role: "assistant", content: "ok" }, finish_reason: "stop" }],
  usage: { prompt_tokens: 20, completion_tokens: 10, total_tokens: 30 },
});

http
  .createServer((req, res) => {
    req.resume();
    req.on("end", () => {
      const send = () => {
        res.writeHead(200, { "content-type": "application/json", "content-length": Buffer.byteLength(reply) });
        res.end(reply);
      };
      delay ? setTimeout(send, delay) : send();
    });
  })
  .listen(port, "127.0.0.1");
