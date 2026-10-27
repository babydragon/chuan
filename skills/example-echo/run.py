import json, sys

args = json.load(sys.stdin)
text = args.get("text", "")
print(f"echo: {text}")
