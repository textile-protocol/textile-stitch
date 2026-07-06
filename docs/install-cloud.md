# Install — Cloud (AWS)

Stitch can run as an operator-owned cloud bot in your own AWS account. The
self-hosting stack in [deploy/aws](../deploy/aws/README.md) creates a
self-contained ECS Fargate environment you own and control: one service, one
secret, one wallet, no public ingress. The bot reaches Textile the same way any
external operator does, over the public RPC / API / indexer URLs in
your `stitch.toml`.

[![Deploy to AWS](https://img.shields.io/badge/Deploy_to-AWS-FF9900?style=for-the-badge&logo=amazonaws&logoColor=white)](https://console.aws.amazon.com/cloudformation/home?region=us-east-1#/stacks/quickcreate?templateURL=https%3A%2F%2Ftextile-stitch-deploy.s3.us-east-1.amazonaws.com%2Faws%2Fcloudformation.yaml&stackName=stitch-operator)

The button opens CloudFormation in your own AWS account with the Stitch stack
prefilled. Sign in, pick your region (it defaults to `us-east-1`), review the
parameters, and create the stack. It comes up stopped by default.

Then follow [deploy/aws](../deploy/aws/README.md) to:

1. load the wallet secret,
2. run the Permit2 approvals,
3. set the service desired count to `1` when you are ready to run live.

For a plain container instead of the managed stack, see
[install-docker.md](install-docker.md). For configuration reference and tuning,
see [ADVANCED.md](../ADVANCED.md).
