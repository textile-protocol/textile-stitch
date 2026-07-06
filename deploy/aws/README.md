# Self-Host Stitch On AWS

This deploy path is for operator-owned Stitch bots. You run it in your own AWS
account, with your own VPC, secrets, and IAM roles that you own and control. The
bot talks to Textile the same way any external operator does, through the public
RPC/API/indexer URLs in `stitch.toml`.

## What The Stack Creates

- A dedicated VPC. The bot task runs in private subnets with no public IP and
  reaches the internet outbound-only through a NAT gateway.
- One ECS service running one Stitch task.
- A dedicated Secrets Manager secret for the operator key and config.
- A dedicated KMS key for that secret.
- CloudWatch logs, Container Insights, stopped-task alerts, and error-log
  alarms.
- No public listener, no load balancer, no inbound security group rules, and no
  public IP on the task.

The NAT gateway is the one always-on billed component in this stack (roughly
$32/month plus per-GB processing). It is what keeps the wallet-holding task off
a public IP. If you would rather trade that isolation for lower cost, you can
run the task in the public subnets with `AssignPublicIp: ENABLED` and the same
egress-only security group, but a private subnet is the recommended posture for
a funds-holding bot.

The service starts at desired count `0` by default. This is deliberate: update
the secret, run approvals, then start live operation.

## One-Click Button

The "Launch Stack" button in the [main README](../../README.md#cloud-deploy)
opens CloudFormation Quick Create in the operator's own AWS account with this
template prefilled. CloudFormation Quick Create only loads a template hosted in
S3, not a raw GitHub URL, so the template is published to a public S3 object by
the [`publish-template.yml`](../../.github/workflows/publish-template.yml)
workflow on every push to `main` and on each release.

The button points at:

```text
https://console.aws.amazon.com/cloudformation/home?region=us-east-1#/stacks/quickcreate?stackName=stitch-operator&templateURL=https%3A%2F%2Ftextile-stitch-deploy.s3.us-east-1.amazonaws.com%2Faws%2Fcloudformation.yaml
```

The operator can change the region and any parameter in the console before
creating the stack. Nothing in this URL touches their wallet; the secret is
loaded separately after the stack exists (see below).

### Deploy From A Local Checkout

The button is the easy path. You can also deploy the same template directly:

```bash
aws cloudformation deploy \
  --stack-name stitch-operator-a \
  --template-file deploy/aws/cloudformation.yaml \
  --capabilities CAPABILITY_IAM \
  --parameter-overrides BotName=stitch-operator-a DesiredCount=0
```

Create one stack per operator wallet.

## Configure Secrets

Prepare `stitch.toml` locally and keep the private key out of the file. Then
update the stack-created secret:

```bash
secret_arn="$(aws cloudformation describe-stacks \
  --stack-name stitch-operator-a \
  --query 'Stacks[0].Outputs[?OutputKey==`SecretArn`].OutputValue' \
  --output text)"

private_key="$(op read 'op://Trading/Stitch Operator/private key')"
config_toml="$(cat stitch.toml)"

aws secretsmanager put-secret-value \
  --secret-id "$secret_arn" \
  --secret-string "$(jq -n \
    --arg key "$private_key" \
    --arg config "$config_toml" \
    '{STITCH_PRIVATE_KEY:$key,STITCH_CONFIG_TOML:$config}')"

unset private_key config_toml
```

Use your own password manager command in place of `op read`.

## Run Permit2 Approvals

Before live operation, run a one-off ECS task with the `approve` command. The
operator wallet needs native gas for approval transactions.

```bash
cluster="$(aws cloudformation describe-stacks \
  --stack-name stitch-operator-a \
  --query 'Stacks[0].Outputs[?OutputKey==`ClusterName`].OutputValue' \
  --output text)"
task_def="$(aws ecs list-task-definitions \
  --family-prefix stitch-operator-a-stitch \
  --sort DESC \
  --max-items 1 \
  --query 'taskDefinitionArns[0]' \
  --output text)"
subnets="$(aws ec2 describe-subnets \
  --filters Name=tag:Name,Values='stitch-operator-a-private-*' \
  --query 'Subnets[].SubnetId' \
  --output text | tr '\t' ',')"
sg="$(aws ec2 describe-security-groups \
  --filters Name=group-name,Values='stitch-operator-a-task-sg' \
  --query 'SecurityGroups[0].GroupId' \
  --output text)"

aws ecs run-task \
  --cluster "$cluster" \
  --launch-type FARGATE \
  --task-definition "$task_def" \
  --network-configuration "awsvpcConfiguration={subnets=[$subnets],securityGroups=[$sg],assignPublicIp=DISABLED}" \
  --overrides '{"containerOverrides":[{"name":"stitch","command":["stitch","approve","--config","/home/stitch/run/stitch.toml"]}]}'
```

Use `--exact` in the command override if you want capped approvals. Maximum
allowance is operationally simpler; exact allowance has a smaller allowance
blast radius but must be refreshed as fills consume it.

## Start, Pause, And Logs

Start live operation:

```bash
aws ecs update-service \
  --cluster stitch-operator-a-cluster \
  --service stitch-operator-a-stitch \
  --desired-count 1
```

Pause without deleting infrastructure:

```bash
aws ecs update-service \
  --cluster stitch-operator-a-cluster \
  --service stitch-operator-a-stitch \
  --desired-count 0
```

Tail logs:

```bash
aws logs tail /ecs/stitch-operator-a/stitch --follow
```

## Security Boundaries

- Use a dedicated wallet per bot.
- Fund only the inventory that bot is allowed to quote or close with.
- Pin `ContainerImage` to an immutable `sha-*` tag for production operation.
- Keep `DesiredCount=0` while changing config or rotating keys.
- Rotate the wallet if the secret is exposed.
- Deleting the stack does not revoke Permit2 approvals. Revoke approvals or
  retire the wallet separately.

## Long-Term Hosted Model

This stack is the base contract for future customer-owned hosted bots:

1. The customer runs this stack in their own AWS account.
2. A Textile wizard generates `stitch.toml`, validates risk settings, and opens
   a CloudFormation Quick Create link with safe defaults.
3. Optional: the wizard can assume a narrow customer-provided role to update
   only this stack's secret and service count.
4. Stronger custody can replace `STITCH_PRIVATE_KEY` with a remote signer or
   HSM/KMS signing API so Stitch never sees raw key material.
