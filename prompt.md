# Noul question for the Decisions API (no free-text prompt!)

The models get no chat prompt, but a typed request:

```json
{
  "model": "<one of the 5 eval models>",
  "state": {
    "envelope_from": "<...>",
    "envelope_rcpts": ["<...>"],
    "client_addr": "...",
    "helo": "...",
    "email-headers": ["From: ...", "Subject: ...", "... all headers ..."],
    "body_excerpt": "...",
    "body_truncated": false
  },
  "questions": {
    "is_spam": {
      "type": "noul",
      "instructions": "Is this mail spam, phishing or malicious?",
      "criteria": {
        "true": "The mail is unsolicited bulk mail, phishing, scam, fraud, or contains malicious content, links, or attachments.",
        "false": "The mail is legitimate correspondence, an expected newsletter with List-Unsubscribe and aligned authentication, or an expected transactional notification."
      }
    }
  },
  "session_id": "<postfix queue-id>"
}
```

Answer per model (no text to parse):

```json
{"answers": {"is_spam": {"type": "noul", "noul": 0.96}}, "usage": {"cost": 0.00002, ...}}
```

`noul` = yes probability 0-1 (`is_spam.noul >= 0.5` → yes).
Question/criteria live in `ai-classification-milter.toml` under `[openrouter.question]`
(`QUESTION_INSTRUCTIONS`, `QUESTION_TRUE`, `QUESTION_FALSE` as env overrides).
