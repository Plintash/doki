#!/bin/sh
# Provider-free app-server fixture: never invokes Codex or any model.
fixture_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
review_turn=review-turn
while IFS= read -r request; do
    printf '%s\n' "$request" >> "$fixture_dir/requests.jsonl"
    id=$(printf '%s' "$request" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
    case "$request" in
        *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id" ;;
        *'"method":"initialized"'*) ;;
        *'"method":"thread/start"'*)
            printf '{"id":%s,"result":{"thread":{"id":"review-thread","turns":[]}}}\n' "$id"
            ;;
        *'"method":"thread/resume"'*)
            printf '{"id":%s,"result":{"thread":{"id":"review-thread","turns":[{"id":"prior-review","items":[{"type":"exitedReviewMode","review":"Prior findings"}]}]}}}\n' "$id"
            ;;
        *'"method":"review/start"'*)
            case "$request" in
                *'REFUSE_REVIEW'*)
                    printf '{"id":%s,"error":{"code":-32601,"message":"Review is unavailable"}}\n' "$id"
                    continue
                    ;;
            esac
            printf '{"id":%s,"result":{"reviewThreadId":"review-thread","turn":{"id":"%s","status":"inProgress"}}}\n' "$id" "$review_turn"
            printf '{"method":"turn/started","params":{"threadId":"review-thread","turn":{"id":"%s"}}}\n' "$review_turn"
            printf '{"method":"item/started","params":{"threadId":"review-thread","turnId":"%s","item":{"type":"enteredReviewMode","id":"enter","review":"Issue scope"}}}\n' "$review_turn"
            case "$request" in
                *'WAIT_FOR_CANCEL'*) continue ;;
            esac
            printf '{"method":"item/started","params":{"threadId":"review-thread","turnId":"%s","item":{"type":"exitedReviewMode","id":"exit","review":"No blocking findings."}}}\n' "$review_turn"
            printf '{"method":"item/completed","params":{"threadId":"review-thread","turnId":"%s","item":{"type":"exitedReviewMode","id":"exit","review":"No blocking findings."}}}\n' "$review_turn"
            printf '{"method":"turn/completed","params":{"threadId":"review-thread","turn":{"id":"%s","status":"completed"}}}\n' "$review_turn"
            ;;
        *'"method":"turn/interrupt"'*)
            printf '{"id":%s,"result":{}}\n' "$id"
            printf '{"method":"item/completed","params":{"threadId":"review-thread","turnId":"%s","item":{"type":"exitedReviewMode","id":"exit","review":"Partial findings."}}}\n' "$review_turn"
            printf '{"method":"turn/completed","params":{"threadId":"review-thread","turn":{"id":"%s","status":"interrupted"}}}\n' "$review_turn"
            ;;
        *) printf '{"id":%s,"error":{"code":-32601,"message":"method not found"}}\n' "$id" ;;
    esac
done
