locals {
  bench_tags         = { Purpose = "bench" }
  bench_instance     = "r7i.2xlarge"
  canonical_account  = "099720109477"
  github_environment = "repo:kazu-2020/epm:environment:bench"
}

resource "aws_vpc" "bench" {
  cidr_block = "10.10.0.0/24"
  tags       = merge(local.bench_tags, { Name = "bench" })
}

resource "aws_internet_gateway" "bench" {
  vpc_id = aws_vpc.bench.id
  tags   = merge(local.bench_tags, { Name = "bench" })
}

resource "aws_subnet" "bench" {
  vpc_id                  = aws_vpc.bench.id
  cidr_block              = aws_vpc.bench.cidr_block
  map_public_ip_on_launch = true
  tags                    = merge(local.bench_tags, { Name = "bench" })
}

resource "aws_route_table" "bench" {
  vpc_id = aws_vpc.bench.id
  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.bench.id
  }
  tags = merge(local.bench_tags, { Name = "bench" })
}

resource "aws_route_table_association" "bench" {
  subnet_id      = aws_subnet.bench.id
  route_table_id = aws_route_table.bench.id
}

resource "aws_security_group" "bench" {
  name        = "bench"
  description = "Outbound only. The instance has no SSH and no inbound port."
  vpc_id      = aws_vpc.bench.id
  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
  tags = local.bench_tags
}

resource "aws_s3_bucket" "bench" {
  bucket = "bench-${data.aws_caller_identity.current.account_id}"
  tags   = local.bench_tags
}

resource "aws_s3_bucket_public_access_block" "bench" {
  bucket                  = aws_s3_bucket.bench.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_lifecycle_configuration" "bench" {
  bucket = aws_s3_bucket.bench.id
  rule {
    id     = "expire"
    status = "Enabled"
    filter {}
    expiration {
      days = 7
    }
    abort_incomplete_multipart_upload {
      days_after_initiation = 1
    }
  }
}

resource "aws_iam_role" "bench_instance" {
  name = "bench-instance"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Service = "ec2.amazonaws.com" }
      Action    = "sts:AssumeRole"
    }]
  })
  tags = local.bench_tags
}

resource "aws_iam_role_policy" "bench_instance" {
  role = aws_iam_role.bench_instance.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = "s3:ListBucket"
        Resource = aws_s3_bucket.bench.arn
        Condition = {
          StringLike = { "s3:prefix" = "bin/*" }
        }
      },
      {
        Effect   = "Allow"
        Action   = "s3:GetObject"
        Resource = "${aws_s3_bucket.bench.arn}/bin/*"
      },
      {
        Effect   = "Allow"
        Action   = "s3:PutObject"
        Resource = "${aws_s3_bucket.bench.arn}/results/*"
      },
    ]
  })
}

resource "aws_iam_instance_profile" "bench" {
  name = "bench-instance"
  role = aws_iam_role.bench_instance.name
  tags = local.bench_tags
}

resource "aws_launch_template" "bench" {
  name                                 = "bench"
  image_id                             = "resolve:ssm:/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id"
  instance_type                        = local.bench_instance
  instance_initiated_shutdown_behavior = "terminate"

  network_interfaces {
    subnet_id                   = aws_subnet.bench.id
    security_groups             = [aws_security_group.bench.id]
    associate_public_ip_address = true
    delete_on_termination       = true
  }

  iam_instance_profile {
    arn = aws_iam_instance_profile.bench.arn
  }

  block_device_mappings {
    device_name = "/dev/sda1"
    ebs {
      volume_size           = 30
      volume_type           = "gp3"
      delete_on_termination = true
      encrypted             = true
    }
  }

  metadata_options {
    http_tokens = "required"
  }

  tag_specifications {
    resource_type = "instance"
    tags          = merge(local.bench_tags, { Name = "bench" })
  }

  tag_specifications {
    resource_type = "volume"
    tags          = local.bench_tags
  }

  tags = local.bench_tags
}

resource "aws_iam_openid_connect_provider" "github" {
  url            = "https://token.actions.githubusercontent.com"
  client_id_list = ["sts.amazonaws.com"]
}

resource "aws_iam_role" "bench_github" {
  name = "bench-github"
  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Effect    = "Allow"
      Principal = { Federated = aws_iam_openid_connect_provider.github.arn }
      Action    = "sts:AssumeRoleWithWebIdentity"
      Condition = {
        StringEquals = {
          "token.actions.githubusercontent.com:aud" = "sts.amazonaws.com"
          "token.actions.githubusercontent.com:sub" = local.github_environment
        }
      }
    }]
  })
  max_session_duration = 10800
  tags                 = local.bench_tags
}

resource "aws_iam_role_policy" "bench_github" {
  role = aws_iam_role.bench_github.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = "ec2:RunInstances"
        Resource = "arn:aws:ec2:ap-northeast-1:${data.aws_caller_identity.current.account_id}:instance/*"
        Condition = {
          ArnEquals    = { "ec2:LaunchTemplate" = aws_launch_template.bench.arn }
          StringEquals = { "ec2:InstanceType" = local.bench_instance }
        }
      },
      {
        Effect = "Allow"
        Action = "ec2:RunInstances"
        Resource = [
          aws_launch_template.bench.arn,
          aws_subnet.bench.arn,
          aws_security_group.bench.arn,
          "arn:aws:ec2:ap-northeast-1:${data.aws_caller_identity.current.account_id}:volume/*",
          "arn:aws:ec2:ap-northeast-1:${data.aws_caller_identity.current.account_id}:network-interface/*",
        ]
      },
      {
        Effect    = "Allow"
        Action    = "ec2:RunInstances"
        Resource  = "arn:aws:ec2:ap-northeast-1::image/*"
        Condition = { StringEquals = { "ec2:Owner" = local.canonical_account } }
      },
      {
        Effect = "Allow"
        Action = "ec2:CreateTags"
        Resource = [
          "arn:aws:ec2:ap-northeast-1:${data.aws_caller_identity.current.account_id}:instance/*",
          "arn:aws:ec2:ap-northeast-1:${data.aws_caller_identity.current.account_id}:volume/*",
        ]
        Condition = { StringEquals = { "ec2:CreateAction" = "RunInstances" } }
      },
      {
        Effect    = "Allow"
        Action    = "ec2:TerminateInstances"
        Resource  = "arn:aws:ec2:ap-northeast-1:${data.aws_caller_identity.current.account_id}:instance/*"
        Condition = { StringEquals = { "ec2:ResourceTag/Purpose" = "bench" } }
      },
      {
        # EC2 reads the AMI parameter of the launch template with the permissions of the caller.
        Effect   = "Allow"
        Action   = "ssm:GetParameters"
        Resource = "arn:aws:ssm:ap-northeast-1::parameter/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id"
      },
      {
        # DescribeInstances has no resource-level permissions.
        Effect   = "Allow"
        Action   = "ec2:DescribeInstances"
        Resource = "*"
      },
      {
        Effect    = "Allow"
        Action    = "iam:PassRole"
        Resource  = aws_iam_role.bench_instance.arn
        Condition = { StringEquals = { "iam:PassedToService" = "ec2.amazonaws.com" } }
      },
      {
        Effect   = "Allow"
        Action   = "s3:PutObject"
        Resource = "${aws_s3_bucket.bench.arn}/bin/*"
      },
      {
        Effect   = "Allow"
        Action   = "s3:ListBucket"
        Resource = aws_s3_bucket.bench.arn
        Condition = {
          StringLike = { "s3:prefix" = "results/*" }
        }
      },
      {
        Effect   = "Allow"
        Action   = "s3:GetObject"
        Resource = "${aws_s3_bucket.bench.arn}/results/*"
      },
    ]
  })
}

output "bench_role_arn" {
  value = aws_iam_role.bench_github.arn
}

output "bench_bucket" {
  value = aws_s3_bucket.bench.bucket
}
