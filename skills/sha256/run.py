import hashlib, json, sys

text = json.load(sys.stdin)["text"]
print(hashlib.sha256(text.encode("utf-8")).hexdigest())
