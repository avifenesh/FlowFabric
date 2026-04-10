# Learning Guide: AWS Step Functions

**Generated**: 2026-04-09
**Sources**: 40 resources analyzed
**Depth**: deep

## Prerequisites

- Familiarity with AWS fundamentals (IAM, CloudWatch, S3)
- Basic understanding of JSON syntax
- Understanding of serverless concepts (Lambda, event-driven architecture)
- AWS account with appropriate permissions
- (Optional) AWS CDK, SAM, or Terraform for IaC sections

## TL;DR

- AWS Step Functions is a fully managed serverless workflow orchestration service that coordinates AWS services and custom code into visual state machines defined in Amazon States Language (ASL).
- **Standard workflows** provide exactly-once execution for up to 1 year; **Express workflows** offer at-least-once execution at 100K/sec for up to 5 minutes.
- State machines consist of states (Task, Choice, Parallel, Map, Wait, Pass, Succeed, Fail) connected by transitions, with built-in Retry/Catch error handling.
- **Distributed Map** enables massive parallel processing (up to 10,000 concurrent child executions) over S3 datasets.
- Three service integration patterns exist: Request-Response (fire and forget), Run a Job (.sync), and Wait for Callback (.waitForTaskToken).

---

## Core Concepts

### What Is AWS Step Functions?

AWS Step Functions is a managed service for building serverless workflows -- called **state machines** -- that orchestrate AWS services, custom code, and human approvals into reliable, auditable processes. Each workflow is defined in Amazon States Language (ASL), a JSON-based specification, and executed as individual **executions** that can be monitored, debugged, and redriven from failure.

Key terminology:
- **State machine**: The workflow definition containing all states and transitions.
- **State**: An individual step that performs work, makes a decision, or controls flow.
- **Execution**: A running instance of a state machine processing a specific input.
- **Task token**: A unique identifier used in callback patterns to resume paused executions.
- **Map Run**: The resource representing all child executions spawned by a Distributed Map state.

(Sources: AWS Step Functions Developer Guide, AWS product page)

### Amazon States Language (ASL)

ASL is the JSON-based language for defining state machines. A state machine definition contains:

```json
{
  "Comment": "Example state machine",
  "StartAt": "FirstState",
  "States": {
    "FirstState": {
      "Type": "Task",
      "Resource": "arn:aws:states:::lambda:invoke",
      "Arguments": {
        "FunctionName": "my-function"
      },
      "Next": "SecondState"
    },
    "SecondState": {
      "Type": "Succeed"
    }
  }
}
```

**Top-level fields**: `Comment`, `StartAt` (required), `States` (required), `QueryLanguage` (JSONata or JSONPath).

**Query languages**: ASL supports two query languages:
- **JSONPath** (legacy, default): Uses `$.field` syntax, `Parameters`, `ResultPath`, `InputPath`, `OutputPath`, `ResultSelector`.
- **JSONata** (recommended for new state machines): Uses `{% expression %}` syntax, `Arguments`, `Output`, `Assign` for variables.

State machine definitions must be saved with `.asl.json` extension when stored outside the console.

(Sources: ASL documentation, States Language specification)

### State Types

| State | Purpose | Key Fields |
|-------|---------|------------|
| **Task** | Performs work via AWS service or activity | `Resource`, `TimeoutSeconds`, `HeartbeatSeconds`, `Retry`, `Catch` |
| **Choice** | Conditional branching | `Choices`, `Default` |
| **Parallel** | Execute multiple branches concurrently | `Branches`, `Retry`, `Catch` |
| **Map** | Iterate over array items (Inline or Distributed) | `ItemsPath`/`ItemReader`, `ItemProcessor`, `MaxConcurrency` |
| **Wait** | Delay execution | `Seconds`, `Timestamp`, `SecondsPath`, `TimestampPath` |
| **Pass** | Pass input to output, optionally transforming | `Result`, `ResultPath` |
| **Succeed** | Terminal success state | (none required) |
| **Fail** | Terminal failure state | `Error`, `Cause` |

**Transitions**: States connect via `Next` (proceed to named state) or `End: true` (terminate). The `StartAt` field names the entry state. Choice states support multiple `Next` targets plus a `Default`. Execution halts at Succeed, Fail, or any state with `End: true`.

(Sources: ASL documentation, CDK Step Functions module)

### Standard vs Express Workflows

| Aspect | Standard | Express (Async) | Express (Sync) |
|--------|----------|-----------------|----------------|
| **Execution guarantee** | Exactly-once | At-least-once | At-most-once |
| **Max duration** | 1 year | 5 minutes | 5 minutes |
| **State transition rate** | 5,000/sec (burst) | Unlimited | Unlimited |
| **Execution start rate** | 300/sec (refill) | 6,000/sec (refill) | 6,000/sec (refill) |
| **Execution history** | 25,000 events, 90-day retention | Via CloudWatch Logs only | Via CloudWatch Logs only |
| **Integration patterns** | All three | Request-Response only | Request-Response only |
| **Distributed Map** | Supported | Not supported | Not supported |
| **Activities** | Supported | Not supported | Not supported |
| **Pricing** | Per state transition | Per execution + duration + memory | Per execution + duration + memory |

**Workflow type is immutable** -- it cannot be changed after creation.

**When to use Standard**: Long-running processes, payment workflows, exactly-once semantics, auditable executions, callback/sync patterns.

**When to use Express**: High-throughput event processing (IoT ingestion, streaming), short-duration tasks under 5 minutes, idempotent operations.

(Sources: Standard vs Express documentation, pricing page)

---

## Service Integration Patterns

Step Functions integrates with 220+ AWS services. Three integration patterns control how tasks interact:

### 1. Request-Response (Default)

Calls the service and immediately proceeds after receiving the HTTP response. Available for both Standard and Express workflows.

```json
{
  "Type": "Task",
  "Resource": "arn:aws:states:::sns:publish",
  "Arguments": {
    "TopicArn": "arn:aws:sns:us-east-1:123456789012:myTopic",
    "Message": "Hello from Step Functions"
  },
  "Next": "NextState"
}
```

### 2. Run a Job (.sync)

Appends `.sync` to the resource ARN. The workflow pauses and waits for the job to complete before proceeding. **Standard workflows only.**

```json
{
  "Type": "Task",
  "Resource": "arn:aws:states:::batch:submitJob.sync",
  "Arguments": {
    "JobDefinition": "arn:aws:batch:us-east-1:123456789012:job-definition/myJob",
    "JobName": "myJobRun",
    "JobQueue": "arn:aws:batch:us-east-1:123456789012:job-queue/myQueue"
  },
  "Next": "ProcessResults"
}
```

Supported by: ECS/Fargate, Glue, Batch, EMR, Athena, CodeBuild, SageMaker, Bedrock, Step Functions (nested).

### 3. Wait for Callback (.waitForTaskToken)

Appends `.waitForTaskToken` to the resource ARN. The workflow pauses and passes a unique task token to the integrated service. Execution resumes when an external process calls `SendTaskSuccess` or `SendTaskFailure` with that token. **Standard workflows only.** Can wait up to 1 year.

```json
{
  "Type": "Task",
  "Resource": "arn:aws:states:::sqs:sendMessage.waitForTaskToken",
  "Arguments": {
    "QueueUrl": "https://sqs.us-east-1.amazonaws.com/123456789012/myQueue",
    "MessageBody": {
      "Input": "{% $states.input.message %}",
      "TaskToken": "{% $states.context.Task.Token %}"
    }
  },
  "HeartbeatSeconds": 600,
  "Next": "ProcessCallback"
}
```

Use cases: Human approvals, third-party system integration, manual review steps.

(Sources: Service integration patterns documentation, SQS/SNS/ECS integration docs)

### Key Service Integrations

| Service | Request-Response | .sync | .waitForTaskToken | Notes |
|---------|:---:|:---:|:---:|-------|
| **Lambda** | Yes | -- | Yes | Optimized payload handling, auto JSON parsing |
| **ECS/Fargate** | Yes | Yes | Yes | Full integration; container overrides supported |
| **DynamoDB** | Yes | -- | -- | GetItem, PutItem, UpdateItem, DeleteItem only |
| **SQS** | Yes | -- | Yes | SendMessage; callback via task token in message body |
| **SNS** | Yes | -- | Yes | Publish; callback via task token |
| **Bedrock** | Yes | Yes | Yes | InvokeModel, CreateModelCustomizationJob |
| **Glue** | Yes | Yes | -- | StartJobRun |
| **API Gateway** | Yes | -- | Yes | REST/HTTP APIs; IAM, Lambda, or no auth |
| **EventBridge** | Yes | -- | Yes | PutEvents; auto-appends execution ARN |
| **Step Functions** | Yes | Yes | Yes | Nested workflows |

(Sources: Supported services documentation, individual service integration pages)

---

## Distributed Map State

The Distributed Map state is a processing mode of the Map state for large-scale parallel workloads. Instead of running iterations inline, it spawns individual **child workflow executions**, each with its own execution history.

### When to Use Distributed Map (vs Inline Map)

Use Distributed Map when:
- Dataset exceeds 256 KiB
- Execution history would exceed 25,000 events
- You need more than 40 concurrent iterations
- You need to process large S3 datasets (CSV, JSON)

### Architecture

```
Parent Workflow
  |
  v
Distributed Map State
  |-- ItemReader (reads from S3 or state input)
  |-- ItemBatcher (optional: group items into batches)
  |-- ItemProcessor (child workflow definition)
  |     |-- ProcessorConfig: { Mode: "DISTRIBUTED", ExecutionType: "EXPRESS" }
  |     |-- StartAt / States (child workflow states)
  |-- ResultWriter (optional: export results to S3)
  |-- MaxConcurrency (default 10,000)
  |-- ToleratedFailurePercentage / ToleratedFailureCount
```

### Complete Example

```json
{
  "ProcessCSV": {
    "Type": "Map",
    "ItemReader": {
      "ReaderConfig": {
        "InputType": "CSV",
        "CSVHeaderLocation": "FIRST_ROW"
      },
      "Resource": "arn:aws:states:::s3:getObject",
      "Parameters": {
        "Bucket": "my-data-bucket",
        "Key": "dataset/records.csv"
      }
    },
    "ItemProcessor": {
      "ProcessorConfig": {
        "Mode": "DISTRIBUTED",
        "ExecutionType": "EXPRESS"
      },
      "StartAt": "ProcessRecord",
      "States": {
        "ProcessRecord": {
          "Type": "Task",
          "Resource": "arn:aws:states:::lambda:invoke",
          "Arguments": {
            "Payload.$": "$",
            "FunctionName": "arn:aws:lambda:us-east-1:123456789012:function:processRecord"
          },
          "End": true
        }
      }
    },
    "MaxConcurrency": 1000,
    "ToleratedFailurePercentage": 5,
    "ResultWriter": {
      "Resource": "arn:aws:states:::s3:putObject",
      "Parameters": {
        "Bucket": "my-results-bucket",
        "Prefix": "output"
      }
    },
    "End": true
  }
}
```

### Key Configuration

- **ItemReader**: Reads from S3 (CSV, JSON) or state input. Supports `CSVHeaderLocation: "FIRST_ROW"`.
- **ItemBatcher**: Groups items into batches (`BatchSize: 100`) for more efficient processing.
- **MaxConcurrency**: Up to 10,000 (default); use `MaxConcurrencyPath` for dynamic values.
- **ToleratedFailurePercentage** (0-100) / **ToleratedFailureCount**: Set failure thresholds before the Map Run fails entirely. Error: `States.ExceedToleratedFailureThreshold`.
- **ResultWriter**: Exports results to S3, grouping by succeeded/failed status.
- **ExecutionType**: `EXPRESS` (fast, default) or `STANDARD` (durable).

(Sources: Distributed Map documentation, Map Run monitoring docs)

---

## Error Handling: Retry and Catch

Step Functions provides built-in error handling via `Retry` and `Catch` fields on Task, Parallel, and Map states.

### Predefined Error Codes

| Error Code | Description | Retriable? |
|-----------|-------------|:---:|
| `States.ALL` | Wildcard for all known errors | Yes |
| `States.TaskFailed` | Wildcard for all task errors except timeout | Yes |
| `States.Timeout` | Task exceeded TimeoutSeconds or HeartbeatSeconds | Yes |
| `States.HeartbeatTimeout` | Heartbeat not received within HeartbeatSeconds | Yes |
| `States.Permissions` | Insufficient IAM privileges | Yes |
| `States.Runtime` | Invalid JSONPath, type mismatch | **No** |
| `States.DataLimitExceeded` | Output exceeds 256 KiB | **No** |
| `States.ExceedToleratedFailureThreshold` | Map failure threshold exceeded | Yes |
| `States.Http.Socket` | HTTP task timeout after 60 seconds | Yes |

### Retry Configuration

```json
"Retry": [
  {
    "ErrorEquals": ["Lambda.ServiceException", "Lambda.SdkClientException"],
    "IntervalSeconds": 2,
    "MaxAttempts": 6,
    "BackoffRate": 2.0,
    "MaxDelaySeconds": 60,
    "JitterStrategy": "FULL"
  },
  {
    "ErrorEquals": ["States.ALL"]
  }
]
```

| Field | Default | Description |
|-------|---------|-------------|
| `ErrorEquals` | (required) | Error names to match |
| `IntervalSeconds` | 1 | Seconds before first retry |
| `MaxAttempts` | 3 | Maximum retries (0 = never retry) |
| `BackoffRate` | 2.0 | Multiplier for retry interval |
| `MaxDelaySeconds` | -- | Caps exponential backoff delay |
| `JitterStrategy` | `NONE` | `FULL` adds randomization to intervals |

### Catch Configuration

```json
"Catch": [
  {
    "ErrorEquals": ["CustomError"],
    "ResultPath": "$.error-info",
    "Next": "HandleCustomError"
  },
  {
    "ErrorEquals": ["States.ALL"],
    "Next": "CatchAllHandler"
  }
]
```

**Processing order**: Retry is evaluated first. If all retries are exhausted (or no retrier matches), Catch is evaluated in order. First matching catcher transitions to its `Next` state.

**ResultPath in Catch**: Controls what the error handler receives:
- `"$"` (default): Overwrites input with error output.
- `"$.error-info"`: Appends error as a field on the original input.
- `null`: Discards error output, passes original input.

(Sources: Error handling documentation, best practices guide)

---

## Workflow Variables

Workflow variables (introduced with JSONata support) let you store data in one step and access it in any future step without threading it through every intermediate state.

### Setting Variables

```json
{
  "Type": "Task",
  "Resource": "arn:aws:states:::lambda:invoke",
  "Arguments": { "FunctionName": "getPrice" },
  "Assign": {
    "product": "{% $states.input.order.product %}",
    "currentPrice": "{% $states.result.Payload.current_price %}"
  },
  "Next": "CheckInventory"
}
```

### Scope Rules

- Variables are **workflow-local**: accessible from any state in the same States block.
- Parallel/Map branches can **read** outer-scope variables but create their own scope for assignments.
- Distributed Map children **cannot** access outer-scope variables.
- Maximum per variable: 256 KiB. Maximum per execution: 10 MiB.

### Reserved Variable: $states

```
$states.input    -- Original input to the state
$states.result   -- API or sub-workflow result (if successful)
$states.errorOutput -- Error output (only in Catch blocks)
$states.context  -- Context object with execution metadata
```

(Sources: Workflow variables documentation)

---

## Input/Output Processing (JSONPath)

For state machines using JSONPath, data flows through a processing pipeline at each state:

```
State Input
  -> InputPath (select subset of input)
    -> Parameters (construct new input with static/dynamic values)
      -> [Task executes]
        -> ResultSelector (reshape task result)
          -> ResultPath (merge result into original input)
            -> OutputPath (select subset for output)
              -> State Output
```

### Intrinsic Functions (JSONPath only)

| Function | Purpose | Example |
|----------|---------|---------|
| `States.Format` | String interpolation | `States.Format('Hello, {}', $.name)` |
| `States.StringToJson` | Parse JSON string | `States.StringToJson($.escapedJson)` |
| `States.JsonToString` | Serialize to JSON string | `States.JsonToString($.obj)` |
| `States.Array` | Create array | `States.Array($.a, $.b)` |
| `States.ArrayPartition` | Chunk array | `States.ArrayPartition($.arr, 4)` |
| `States.ArrayContains` | Check membership | `States.ArrayContains($.arr, $.val)` |
| `States.ArrayRange` | Generate range | `States.ArrayRange(1, 10, 2)` |
| `States.ArrayGetItem` | Index access | `States.ArrayGetItem($.arr, 0)` |
| `States.ArrayLength` | Array size | `States.ArrayLength($.arr)` |
| `States.ArrayUnique` | Deduplicate | `States.ArrayUnique($.arr)` |
| `States.Base64Encode` | Base64 encode | `States.Base64Encode($.data)` |
| `States.Base64Decode` | Base64 decode | `States.Base64Decode($.encoded)` |
| `States.Hash` | Hash (MD5, SHA-*) | `States.Hash($.data, 'SHA-256')` |
| `States.JsonMerge` | Merge objects | `States.JsonMerge($.a, $.b, false)` |
| `States.MathAdd` | Integer addition | `States.MathAdd($.val, 1)` |
| `States.MathRandom` | Random integer | `States.MathRandom(1, 100)` |
| `States.StringSplit` | Split string | `States.StringSplit($.csv, ',')` |
| `States.UUID` | Generate UUID v4 | `States.UUID()` |

(Sources: Intrinsic functions documentation)

---

## Amazon Bedrock Integration

Step Functions integrates with Amazon Bedrock for AI/ML workflows:

### InvokeModel

```json
{
  "Type": "Task",
  "Resource": "arn:aws:states:::bedrock:invokeModel",
  "Arguments": {
    "ModelId": "amazon.titan-text-express-v1",
    "Body": {
      "inputText": "{% $states.input.prompt %}",
      "textGenerationConfig": {
        "maxTokenCount": 500,
        "temperature": 0.7
      }
    },
    "ContentType": "application/json",
    "Accept": "application/json"
  },
  "Next": "ProcessResponse"
}
```

### Prompt Chaining Pattern

Chain multiple Bedrock invocations where each step builds on the previous:

```
Generate Outline -> Expand Section 1 -> Expand Section 2 -> Merge & Edit
```

Each step uses the previous result as context input for the next prompt.

### AI Agent Patterns

Step Functions can orchestrate agentic workflows by:
1. Invoking Bedrock models with tool-use capabilities
2. Using Choice states to route based on model responses
3. Implementing human-in-the-loop with callback patterns
4. Managing conversation state across multiple model invocations

(Sources: Bedrock integration docs, prompt chaining sample)

---

## Callback Patterns

The `.waitForTaskToken` pattern enables Step Functions to pause and wait for external systems to complete work.

### Human Approval Pattern

```json
{
  "WaitForApproval": {
    "Type": "Task",
    "Resource": "arn:aws:states:::sqs:sendMessage.waitForTaskToken",
    "Arguments": {
      "QueueUrl": "https://sqs.us-east-1.amazonaws.com/123456789012/approval-queue",
      "MessageBody": {
        "RequestDetails": "{% $states.input %}",
        "TaskToken": "{% $states.context.Task.Token %}"
      }
    },
    "HeartbeatSeconds": 3600,
    "Next": "ProcessApproval"
  }
}
```

The external system (web app, Lambda consumer, etc.) calls:
```python
sfn_client.send_task_success(
    taskToken=task_token,
    output='{"approved": true, "reviewer": "jane@example.com"}'
)
```

Or on rejection:
```python
sfn_client.send_task_failure(
    taskToken=task_token,
    error="RejectedError",
    cause="Request denied by reviewer"
)
```

(Sources: Callback pattern documentation, SQS integration)

---

## Nested Workflows

State machines can invoke other state machines, enabling modular, reusable workflow libraries.

### Asynchronous (Fire and Forget)

```json
{
  "Type": "Task",
  "Resource": "arn:aws:states:::states:startExecution",
  "Arguments": {
    "StateMachineArn": "arn:aws:states:us-east-1:123456789012:stateMachine:ChildWorkflow",
    "Input": {
      "data": "{% $states.input %}",
      "AWS_STEP_FUNCTIONS_STARTED_BY_EXECUTION_ID": "{% $states.context.Execution.Id %}"
    }
  },
  "End": true
}
```

### Synchronous (Wait for Completion)

```json
{
  "Type": "Task",
  "Resource": "arn:aws:states:::states:startExecution.sync:2",
  "Arguments": {
    "StateMachineArn": "arn:aws:states:us-east-1:123456789012:stateMachine:ChildWorkflow",
    "Input": "{% $states.input %}"
  },
  "Next": "ProcessChildResult"
}
```

Use `AWS_STEP_FUNCTIONS_STARTED_BY_EXECUTION_ID` to link parent-child in the console.

(Sources: Nested workflows documentation)

---

## Versioning, Aliases, and Redrives

### Versions and Aliases

- **Version**: A numbered, immutable snapshot of a state machine that can be executed independently.
- **Alias**: A pointer that can route traffic to up to two versions, enabling blue/green and canary deployments.
- **Revision**: An internal immutable snapshot (UUID) that cannot be directly executed.

Limits: 1,000 versions and 100 aliases per state machine.

### Execution Redrives

Redrives restart failed Standard workflow executions from the point of failure:

```bash
aws stepfunctions redrive-execution \
  --execution-arn arn:aws:states:us-east-1:123456789012:execution:myWorkflow:exec-123
```

**Key behaviors**:
- Successful steps are skipped (not rerun).
- Retry counts reset to 0.
- Must be within 14 days of completion.
- Only for non-SUCCEEDED executions started after November 15, 2023.
- Redriven transitions count toward billing.

(Sources: Versioning/aliases docs, redrives documentation)

---

## Activities

Activities enable external workers (EC2, ECS, on-premises, mobile) to perform work for Step Functions using a **polling model**:

1. Worker calls `GetActivityTask` with activity ARN
2. Receives JSON input and a `taskToken`
3. Performs work
4. Reports results via `SendTaskSuccess` or `SendTaskFailure`
5. Sends periodic `SendTaskHeartbeat` for long-running tasks

Activities can wait up to 1 year. They are Standard-workflow only and are not versioned.

Best practice: Maintain at least 100 open polls per activity ARN.

(Sources: Activities documentation)

---

## Infrastructure as Code

### AWS CDK (TypeScript)

```typescript
import * as sfn from 'aws-cdk-lib/aws-stepfunctions';
import * as tasks from 'aws-cdk-lib/aws-stepfunctions-tasks';

const submitJob = new tasks.LambdaInvoke(this, 'SubmitJob', {
  lambdaFunction: submitLambda,
  outputPath: '$.Payload',
});

const waitStep = new sfn.Wait(this, 'WaitForCompletion', {
  time: sfn.WaitTime.duration(Duration.seconds(30)),
});

const checkStatus = new tasks.LambdaInvoke(this, 'CheckStatus', {
  lambdaFunction: checkLambda,
  outputPath: '$.Payload',
});

const jobFailed = new sfn.Fail(this, 'JobFailed', {
  error: 'JobFailed',
  cause: 'Processing failed',
});

const jobSucceeded = new sfn.Succeed(this, 'JobSucceeded');

const definition = submitJob
  .next(waitStep)
  .next(checkStatus)
  .next(new sfn.Choice(this, 'JobComplete?')
    .when(sfn.Condition.stringEquals('$.status', 'FAILED'), jobFailed)
    .when(sfn.Condition.stringEquals('$.status', 'SUCCEEDED'), jobSucceeded)
    .otherwise(waitStep));

new sfn.StateMachine(this, 'ProcessingPipeline', {
  definitionBody: sfn.DefinitionBody.fromChainable(definition),
  timeout: Duration.hours(1),
  tracingEnabled: true,
});
```

CDK features: `addRetry()`, `addCatch()`, `Chain.start()`, `DefinitionBody.fromString()`, `DistributedMap`, JSONata support via `*.jsonata()` factory methods.

### AWS SAM

```yaml
Resources:
  ProcessingStateMachine:
    Type: AWS::Serverless::StateMachine
    Properties:
      DefinitionUri: statemachine/processing.asl.json
      Type: STANDARD
      Tracing:
        Enabled: true
      Logging:
        Level: ERROR
        IncludeExecutionData: true
        Destinations:
          - CloudWatchLogsLogGroup:
              LogGroupArn: !GetAtt StateMachineLogGroup.Arn
      Policies:
        - LambdaInvokePolicy:
            FunctionName: !Ref ProcessFunction
        - DynamoDBCrudPolicy:
            TableName: !Ref DataTable
      DefinitionSubstitutions:
        ProcessFunctionArn: !GetAtt ProcessFunction.Arn
        DataTableName: !Ref DataTable
      Events:
        ScheduleEvent:
          Type: Schedule
          Properties:
            Schedule: rate(1 hour)
```

Key SAM features: `DefinitionSubstitutions` for injecting resource ARNs, `AutoPublishAlias` for continuous deployments, event source triggers.

### Terraform

```hcl
resource "aws_sfn_state_machine" "processing" {
  name     = "processing-workflow"
  role_arn = aws_iam_role.step_functions.arn
  type     = "STANDARD"

  definition = templatefile("${path.module}/statemachine/processing.asl.json", {
    lambda_arn  = aws_lambda_function.process.arn
    table_name  = aws_dynamodb_table.data.name
  })

  logging_configuration {
    log_destination        = "${aws_cloudwatch_log_group.sfn.arn}:*"
    include_execution_data = true
    level                  = "ERROR"
  }

  tracing_configuration {
    enabled = true
  }

  tags = {
    Environment = "production"
    Application = "data-processing"
  }
}

resource "aws_iam_role" "step_functions" {
  name = "step-functions-execution-role"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Action = "sts:AssumeRole"
      Effect = "Allow"
      Principal = { Service = "states.amazonaws.com" }
    }]
  })
}
```

(Sources: CDK Step Functions module, SAM StateMachine resource, Terraform registry)

---

## Pricing Model

### Standard Workflows

- **$0.000025 per state transition** (US East)
- Free tier: **4,000 state transitions/month** (does not expire)
- Each step, retry, and redrive counts as a transition
- Distributed Map: one transition per child execution started

### Express Workflows

- **$1.00 per million requests**
- **$0.00001667 per GB-second** of duration
- Memory: 50 MB baseline + definition size + execution data, billed in 64 MB chunks

### Cost Optimization Strategies

1. **Nest Express within Standard**: Use Standard for orchestration, Express for high-volume sub-workflows.
2. **Reduce state transitions**: Combine logic in Lambda instead of multiple Pass/Choice states.
3. **Use Distributed Map**: One transition per child instead of per-item transitions.
4. **Use Pass states**: Avoid unnecessary Lambda invocations for simple transformations.

(Sources: Pricing page, best practices documentation)

---

## Workflow Studio

Workflow Studio is the visual IDE within the Step Functions console:

### Three Modes

1. **Design Mode**: Drag-and-drop states from a browser (Actions, Flow, Patterns tabs) onto a canvas. Inspector panel configures each state.
2. **Code Mode**: Full ASL JSON editor with syntax highlighting, auto-completion, and real-time graph visualization. Supports keyboard shortcuts (Ctrl+Space for field suggestions).
3. **Config Mode**: Set workflow name, type, IAM role, logging, X-Ray tracing, encryption, and tags.

Changes synchronize automatically between Design and Code modes.

### Integration with IaC

- Use Workflow Studio within **AWS Infrastructure Composer** to generate SAM/CloudFormation templates.
- Export ASL definitions in JSON or YAML for version control.
- VS Code extension (AWS Toolkit) supports visualization and individual state testing.

(Sources: Workflow Studio documentation)

---

## Observability

### CloudWatch Logs

| Log Level | Events Logged |
|-----------|---------------|
| `ALL` | All state entries, exits, task results, and failures |
| `ERROR` | Task failures, Lambda errors, timeouts |
| `FATAL` | Only critical execution failures |
| `OFF` | No logging |

Express workflows require CloudWatch Logs for execution history. Standard workflows have built-in history but benefit from log-based analysis.

Best practice: Prefix log groups with `/aws/vendedlogs/states/` to avoid resource policy limits.

### CloudWatch Metrics

Key metrics to monitor:

| Metric | Statistic | Alert Threshold |
|--------|-----------|-----------------|
| `ExecutionsFailed` | Sum | > 0 |
| `ExecutionsTimedOut` | Sum | > 0 |
| `ExecutionThrottled` | Sum | > 0 |
| `ExecutionTime` | Average, p99 | SLA-dependent |
| `OpenExecutionCount` | Maximum | < 900,000 |

### AWS X-Ray

Enable tracing for service maps and latency analysis:

```json
{
  "TracingConfiguration": {
    "enabled": true
  }
}
```

X-Ray shows trace segments per state, service dependencies, and error rates. **Limitation**: Not supported for Distributed Map child executions (trace document size limits).

Default sampling: First request/second + 5% of additional requests.

(Sources: CloudWatch Logs docs, metrics reference, X-Ray tracing docs)

---

## Testing

### TestState API

Test individual states in isolation without deploying:

```bash
aws stepfunctions test-state \
  --definition '{"Type":"Task","Resource":"arn:aws:states:::lambda:invoke","Arguments":{"FunctionName":"myFunc","Payload.$":"$"},"End":true}' \
  --role-arn arn:aws:iam::123456789012:role/myRole \
  --input '{"key": "value"}' \
  --inspection-level DEBUG
```

**Inspection levels**:
- `INFO`: State output or error details.
- `DEBUG`: Full input/output processing pipeline (InputPath, Parameters, ResultSelector, ResultPath, OutputPath).
- `TRACE`: Raw HTTP request/response (HTTP tasks only).

**Mocking**: Test without invoking real services:
```bash
--mock '{"result": "{\"Payload\": {\"statusCode\": 200}}"}'
```

Can test retry/catch behavior, Map/Parallel states, and validate data transformations.

### Step Functions Local

Docker-based local testing environment (unsupported, limited feature parity):

```bash
docker pull amazon/aws-stepfunctions-local
docker run -p 8083:8083 amazon/aws-stepfunctions-local

aws stepfunctions --endpoint-url http://localhost:8083 \
  create-state-machine --definition file://workflow.asl.json \
  --name TestWorkflow --role-arn arn:aws:iam::012345678901:role/DummyRole
```

Supports local Lambda (via SAM CLI), DynamoDB Local, and mocked service endpoints.

(Sources: TestState API documentation, Step Functions Local docs)

---

## Security

### IAM Execution Role

Every state machine requires an execution role. Follow least-privilege:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["lambda:InvokeFunction"],
      "Resource": [
        "arn:aws:lambda:us-east-1:123456789012:function:myFunction"
      ]
    }
  ]
}
```

### Tag-Based Access Control

```json
{
  "Effect": "Deny",
  "Action": ["states:DeleteStateMachine", "states:StopExecution"],
  "Resource": "*",
  "Condition": {
    "StringEquals": { "aws:ResourceTag/environment": "production" }
  }
}
```

### Encryption

- **At rest**: Encrypted by default with AWS-owned keys. Optionally use customer-managed KMS keys via `encryptionConfiguration`.
- **In transit**: All API calls use HTTPS/TLS.

### VPC Endpoints

Interface VPC endpoints (PrivateLink) allow Step Functions API access from within a VPC without traversing the public internet.

(Sources: Best practices documentation, tagging guide)

---

## Service Quotas

| Resource | Standard | Express |
|----------|----------|---------|
| Max execution duration | 1 year | 5 minutes |
| Max execution history | 25,000 events | Unlimited |
| Max payload size | 256 KiB | 256 KiB |
| Max definition size | 1 MB | 1 MB |
| State machines per account | 100,000 | 100,000 |
| Open executions per account | 1,000,000 | -- |
| Open Map Runs | 1,000 (hard) | -- |
| Max parallel Map children | 10,000 (hard) | -- |
| Versions per state machine | 1,000 | 1,000 |
| Aliases per state machine | 100 | 100 |
| Tags per resource | 50 | 50 |
| State transition burst (US East) | 5,000 | Unlimited |
| State transition refill (US East) | 5,000/sec | Unlimited |
| StartExecution refill (US East) | 300/sec | 6,000/sec |

**Hard quotas** (cannot be increased): payload size, definition size, Map Runs, parallel children, HTTP timeout (60s).

(Sources: Service quotas documentation)

---

## Common Pitfalls

| Pitfall | Why It Happens | How to Avoid |
|---------|---------------|--------------|
| Payload exceeds 256 KiB | Large data passed between states | Store in S3, pass ARN reference |
| Execution history exceeds 25,000 events | Long-running loops or high-iteration maps | Use Distributed Map or start new executions |
| No TimeoutSeconds configured | Task hangs indefinitely waiting | Always set TimeoutSeconds on Task states |
| Not retrying Lambda transient errors | Lambda service exceptions are common | Add Retry for Lambda.ServiceException, Lambda.SdkClientException |
| Using Express for .sync/.waitForTaskToken | Express only supports Request-Response | Use Standard for sync/callback patterns |
| Workflow type chosen incorrectly | Cannot change type after creation | Carefully evaluate requirements before creating |
| States.ALL not catching DataLimitExceeded | States.ALL excludes terminal errors | Handle States.DataLimitExceeded and States.Runtime explicitly |
| Missing CloudWatch Logs for Express | No execution history without logging | Always enable logging for Express workflows |
| Non-ASCII characters in names | Prevents CloudWatch logging | Use ASCII-only names |
| Activity polling latency | Insufficient concurrent pollers | Maintain 100+ open polls per activity ARN |

(Sources: Best practices documentation, error handling guide, quotas)

---

## Best Practices

1. **Always set TimeoutSeconds** on Task and Activity states to prevent stuck executions. (Source: AWS best practices)
2. **Handle Lambda transient errors** with Retry for `Lambda.ServiceException`, `Lambda.SdkClientException`, `Lambda.TooManyRequestsException`. (Source: AWS best practices)
3. **Use S3 for large payloads** -- store data in S3 and pass the ARN instead of raw data. (Source: AWS best practices)
4. **Nest Express workflows inside Standard** for cost-effective high-throughput sub-workflows. (Source: AWS best practices)
5. **Use Distributed Map** for large dataset processing instead of inline loops to avoid history quota limits. (Source: AWS best practices)
6. **Enable CloudWatch Logs** at minimum `ERROR` level for all workflows; required for Express. (Source: Observability docs)
7. **Enable X-Ray tracing** for production workflows to visualize dependencies and identify bottlenecks. (Source: X-Ray docs)
8. **Tag resources** for cost allocation and tag-based IAM security policies. (Source: AWS best practices)
9. **Use HeartbeatSeconds** for callback patterns and long-running activities. Set heartbeat interval shorter than timeout. (Source: Activities docs)
10. **Prefix log groups** with `/aws/vendedlogs/states/` to avoid CloudWatch resource policy size limits. (Source: AWS best practices)
11. **Test with TestState API** before deploying -- validate data transformations, retry/catch logic, and mock service responses. (Source: TestState docs)
12. **Use workflow variables** (Assign) instead of threading data through every state for cleaner workflows. (Source: Variables docs)

---

## Further Reading

| Resource | Type | Why Recommended |
|----------|------|-----------------|
| [Step Functions Developer Guide](https://docs.aws.amazon.com/step-functions/latest/dg/welcome.html) | Official Docs | Comprehensive reference for all features |
| [Amazon States Language Spec](https://states-language.net/spec.html) | Specification | Formal ASL specification |
| [Step Functions Workshop](https://catalog.workshops.aws/stepfunctions) | Workshop | Hands-on learning with guided exercises |
| [CDK Step Functions Module](https://docs.aws.amazon.com/cdk/api/v2/docs/aws-cdk-lib.aws_stepfunctions-readme.html) | Official Docs | CDK constructs reference |
| [SAM StateMachine Resource](https://docs.aws.amazon.com/serverless-application-model/latest/developerguide/sam-resource-statemachine.html) | Official Docs | SAM template reference |
| [Statelint Validator](https://github.com/awslabs/statelint) | Tool | Validate ASL definitions locally |
| [Serverless Land Patterns](https://serverlessland.com/step-functions) | Patterns | Community patterns and templates |
| [Step Functions Pricing](https://aws.amazon.com/step-functions/pricing/) | Pricing | Detailed pricing and calculator |
| [Bedrock Prompt Chaining Sample](https://docs.aws.amazon.com/step-functions/latest/dg/sample-bedrock-prompt-chaining.html) | Sample | AI workflow orchestration example |
| [Step Functions Quotas](https://docs.aws.amazon.com/step-functions/latest/dg/limits-overview.html) | Reference | Service limits and soft/hard quotas |

---

*This guide was synthesized from 40 sources. See `resources/aws-step-functions-sources.json` for full source list with quality scores.*
