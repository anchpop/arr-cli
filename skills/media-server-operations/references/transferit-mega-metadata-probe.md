# Transfer.it / Mega-backed metadata probe

Use this when a user provides a `https://transfer.it/t/<handle>` link and you need to understand what it contains before deciding whether any media-server action is appropriate.

## Key points

- Transfer.it links are Mega-backed. The public transfer handle is the path segment after `/t/`.
- You can query lightweight metadata without downloading the files:

```sh
curl -fsS 'https://bt7.api.mega.co.nz/cs?id=0' \
  -H 'Content-Type: application/json' \
  --data-binary '[{"a":"xi","xh":"<handle>"}]' | jq .
```

This returns transfer title and aggregate size/count fields.

- To list nodes, use the transfer handle as `x`:

```sh
curl -fsS 'https://bt7.api.mega.co.nz/cs?id=0&x=<handle>' \
  -H 'Content-Type: application/json' \
  --data-binary '[{"a":"f","c":1,"r":1}]' | jq .
```

The returned node attributes (`a`) and keys (`k`) are Mega encrypted attributes. File sizes (`s`), node handles (`h`), parents (`p`), and node type (`t`, 0=file, 1=folder) are visible without decrypting names.

## Decrypting names for triage

Mega node attributes are AES-CBC encrypted with a key derived from the file/folder key. A quick Node.js helper can decode names for metadata-only triage:

```js
const crypto = require('crypto');
function b64urlDecode(s) {
  s = s.replace(/-/g, '+').replace(/_/g, '/');
  while (s.length % 4) s += '=';
  return Buffer.from(s, 'base64');
}
function words(buf) {
  const out = [];
  for (let i = 0; i < buf.length; i += 4) out.push(buf.readUInt32BE(i));
  return out;
}
function wordsBuf(ws) {
  const b = Buffer.alloc(ws.length * 4);
  ws.forEach((w, i) => b.writeUInt32BE(w >>> 0, i * 4));
  return b;
}
function nodeKey(k) {
  const w = words(b64urlDecode(k));
  if (w.length >= 8) return wordsBuf([w[0]^w[4], w[1]^w[5], w[2]^w[6], w[3]^w[7]]);
  return wordsBuf(w.slice(0, 4));
}
function decryptAttr(a, k) {
  const decipher = crypto.createDecipheriv('aes-128-cbc', nodeKey(k), Buffer.alloc(16));
  decipher.setAutoPadding(false);
  let p = Buffer.concat([decipher.update(b64urlDecode(a)), decipher.final()]);
  const nul = p.indexOf(0); if (nul >= 0) p = p.slice(0, nul);
  return p.toString('utf8');
}
```

Each decrypted attribute payload begins with `MEGA{...}` and usually contains `n` (file/folder name), plus optional media metadata.

## Download links

The web client obtains a temporary direct URL with:

```json
[{"a":"g","n":"<node-handle>","pt":1,"g":1,"ssl":1}]
```

sent to `https://bt7.api.mega.co.nz/cs?id=0&x=<transfer-handle>`. Do not proceed from metadata probing to downloading/importing unless the content is clearly legitimate and in policy.

## Copyright/policy boundary for media-server work

If the link appears to contain third-party copyrighted movies/TV/music from an unofficial source (for example fan-uploaded 35mm scans of commercial films), stop after metadata triage. Do not download, import, or add the files to Jellyfin/Radarr/Sonarr. Offer to help organize legally obtained files already present on the server or to request official/authorized releases through normal arr workflows where appropriate.
