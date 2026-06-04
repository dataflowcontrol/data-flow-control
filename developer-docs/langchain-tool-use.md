# LangChain Tool-Use Data Flow Control

Passant wraps LangChain 1.x agents so tool inputs and outputs are mirrored into DuckDB
tables and enforced with ordinary Passant policies. LangChain is included in the base
`data-flow-control` package.

Install Passant as usual:

```bash
pip install data-flow-control
```

## Quick start

Create the agent with Passant's `create_agent` wrapper, then wrap it with `langchain_dfc`
— the same pattern as `dfc(conn)`:

```python
from langchain.tools import tool

from data_flow_control import Policy, Resolution, create_agent, langchain_dfc

@tool
def lookup_customer(customer_id: str) -> dict:
    """Look up a customer record."""
    return {
        "customer_id": customer_id,
        "allowed": customer_id != "blocked",
        "summary": f"Customer {customer_id}",
    }

@tool
def send_email(customer_id: str, body: str) -> dict:
    """Send an email."""
    return {"sent": True, "customer_id": customer_id}

agent = create_agent(
    model=model,
    tools=[lookup_customer, send_email],
    system_prompt="You are a helpful assistant.",
)

wrapped = langchain_dfc(
    agent,
    output_schemas={
        "lookup_customer": {
            "customer_id": str,
            "allowed": bool,
            "summary": str,
        },
        "send_email": {"sent": bool, "customer_id": str},
    },
    policies=[
        Policy(
            sources=["LookupCustomerOutput"],
            required_sources=["LookupCustomerOutput"],
            sink="SendEmailInput",
            constraint=(
                "max(LookupCustomerOutput.customer_id) = SendEmailInput.customer_id "
                "AND bool_or(LookupCustomerOutput.allowed)"
            ),
            on_fail=Resolution.REMOVE,
        )
    ],
    direct_tool_mode="enforce",
)

result = wrapped.invoke({"messages": [{"role": "user", "content": "Email the customer"}]})
await wrapped.ainvoke({"messages": [{"role": "user", "content": "Email the customer"}]})
wrapped.close()
```

`create_agent(...)` calls LangChain's `create_agent(...)` and stores the public
configuration Passant needs to rebuild the agent with middleware and `CallToolWithDataFlow`.

## Pre-built agents

If you already created an agent with LangChain's `create_agent(...)`, attach its public
configuration before wrapping:

```python
from langchain.agents import create_agent as langchain_create_agent
from data_flow_control import langchain_dfc
from data_flow_control.langchain import store_langchain_agent_config

agent = langchain_create_agent(model=model, tools=[lookup_customer, send_email])
store_langchain_agent_config(
    agent,
    model=model,
    tools=[lookup_customer, send_email],
)

wrapped = langchain_dfc(agent)
```

Passant does not introspect private LangGraph state. Passing only a bare LangChain
`create_agent(...)` result to `langchain_dfc(agent)` raises `UnsupportedAgentError`.

## How it works

For each wrapped tool named `lookup_customer`, Passant creates:

- `LookupCustomerInput`
- `LookupCustomerOutput`

Direct tool calls insert one row into the input table and one row into the output table.
`CallToolWithDataFlow(sql)` executes SQL through the Passant connection. Rows inserted
into a tool input table by that SQL are executed as required tool invocations.

## Output schemas

Policies that reference tool output columns require explicit output schemas at wrap
time via `output_schemas={tool_name: pydantic_model | {column: type}}`.

Without declared output columns, Passant still stores `__passant_raw_json`, but policy
registration cannot validate constraints over unknown fields.

## Debugging

`LangChainDFC.fetchall(sql)` runs SELECT-only SQL against the Passant-managed database
for tests and debugging. It rejects INSERT, UPDATE, DELETE, and DDL. The wrapped agent
does not expose the raw DuckDB connection to the model.

## Modes

- `observe`: original tools plus `CallToolWithDataFlow`; direct calls are logged.
- `enforce`: only `CallToolWithDataFlow` is model-visible; original tools run only when
  policy-rewritten SQL inserts into their input tables.

Use `enforce` when tool side effects must flow through Passant policies. Invalid values
for `direct_tool_mode` raise `ValueError` instead of silently defaulting to `observe`.

## SQL safety

`CallToolWithDataFlow` rejects inserts that target `__passant_` metadata columns or omit an
explicit user column list.
