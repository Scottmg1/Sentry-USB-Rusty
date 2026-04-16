import argparse
import boto3

def send_sns(topic: str, subject: str, message: str):
    sns = boto3.client('sns')
    response = sns.publish(TopicArn=topic, Message=message, Subject=subject)
    return response

if __name__== "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument('-t', '--topic', help='The SNS topic to publish to')
    parser.add_argument('-m', '--message', help='The message to publish')
    parser.add_argument('-s', '--subject', help='The subject of the message to publish')
    args = parser.parse_args()

    send_sns(args.topic, args.subject, args.message)
