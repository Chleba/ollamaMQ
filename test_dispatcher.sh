#!/bin/bash

# Configuration
BASE_URL="http://localhost:11435"
ENDPOINTS=("/api/generate" "/api/chat" "/v1/chat/completions" "/v1/completions")

# Expanded list of 50 users to thoroughly test scrolling and high load
USERS=(
    "alice" "bob" "charlie" "david" "eve" 
    "frank" "grace" "heidi" "ivan" "judy" 
    "kevin" "laura" "mike" "nancy" "oscar"
    "peggy" "quinn" "ralph" "steve" "trent"
    "ursula" "victor" "walter" "xenia" "yvonne"
    "zelda" "arthur" "beatrice" "clarence" "dorothy"
    "edward" "florence" "george" "harriet" "isaac"
    "jane" "kurt" "lily" "marvin" "nellie"
    "owen" "pearl" "quintin" "rose" "samuel"
    "tessa" "ulysses" "vera" "william" "yasmin"
)

echo "🚀 Starting 50-User Stress Test for ollamaMQ..."
echo "Target Base: $BASE_URL"
echo "Endpoints: ${ENDPOINTS[*]}"
echo "Total Potential Users: ${#USERS[@]}"
echo "----------------------------------------"

# Function to send a request
send_request() {
    local user=$1
    local id=$2
    local endpoint=${ENDPOINTS[$RANDOM % ${#ENDPOINTS[@]}]}
    local url="${BASE_URL}${endpoint}"
    
    local payload=""
    if [[ "$endpoint" == "/api/chat" || "$endpoint" == "/v1/chat/completions" ]]; then
        payload="{\"model\": \"llama3\", \"messages\": [{\"role\": \"user\", \"content\": \"Request $id from $user via $endpoint\"}]}"
    else
        payload="{\"model\": \"llama3\", \"prompt\": \"Request $id from $user via $endpoint\"}"
    fi

    # Send request and capture HTTP status code
    status_code=$(curl -s -o /dev/null -w "%{http_code}" -X POST "$url" \
        -H "X-User-ID: $user" \
        -H "Content-Type: application/json" \
        -d "$payload")

    if [ "$status_code" -eq 200 ]; then
        echo "✅ [SUCCESS] User: $user | Endpoint: $endpoint | Req: $id"
    elif [ "$status_code" -eq 502 ]; then
        echo "⚠️  [BACKEND ERROR] User: $user | Endpoint: $endpoint | Req: $id (Ollama Offline)"
    else
        echo "❌ [FAILED] User: $user | Endpoint: $endpoint | Req: $id | Status: $status_code"
    fi
}

# Function to simulate a client disconnecting early
send_and_cancel() {
    local user=$1
    local id=$2
    local endpoint=${ENDPOINTS[$RANDOM % ${#ENDPOINTS[@]}]}
    local url="${BASE_URL}${endpoint}"
    
    echo "🏃 [CANCEL TEST] User: $user | Req: $id (Will disconnect early)"
    
    # Start curl in background, wait a tiny bit, then kill it
    curl -s -X POST "$url" \
        -H "X-User-ID: $user" \
        -H "Content-Type: application/json" \
        -d "{\"model\": \"llama3\", \"prompt\": \"Canceled request $id\"}" > /dev/null &
    
    local curl_pid=$!
    # Sleep slightly less than the dispatcher's 500ms artificial delay to test 'is_closed' check,
    # or slightly more to test 'tokio::select' abortion during backend call.
    sleep 0.3
    kill $curl_pid 2>/dev/null
}

# Check if dispatcher is reachable (using /api/generate as health check)
if ! curl -s -o /dev/null "${BASE_URL}/api/generate" --max-time 2; then
    echo "❌ Error: Dispatcher is not reachable at ${BASE_URL}"
    echo "   Please run 'cargo run' in another terminal first."
    exit 1
fi

total_dispatched=0

echo "📡 Dispatching randomized requests in the background..."
for user in "${USERS[@]}"; do
    # Randomize number of requests between 1 and 12
    num_reqs=$((1 + RANDOM % 12))
    total_dispatched=$((total_dispatched + num_reqs))
    
    echo "👤 User: $user -> Sending $num_reqs requests..."
    
    for ((i=1; i<=num_reqs; i++)); do
        # 10% chance to simulate a client cancellation
        if [ $((RANDOM % 10)) -eq 0 ]; then
            send_and_cancel "$user" "$i"
        else
            send_request "$user" "$i" &
        fi
    done
    
    # Small sleep between user bursts to stagger the incoming load
    sleep 0.1
done

echo "----------------------------------------"
echo "⏳ Total of $total_dispatched requests dispatched across ${#USERS[@]} users."
echo "⏳ Waiting for all tasks to complete... (Watch the TUI!)"

# Wait for all background processes to finish
wait

echo "----------------------------------------"
echo "🏁 High-load stress test completed."
