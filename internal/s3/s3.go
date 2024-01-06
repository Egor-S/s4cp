package s3

import (
	"context"
	"errors"
	"os"

	"github.com/aws/aws-sdk-go-v2/aws"
	"github.com/aws/aws-sdk-go-v2/aws/transport/http"
	"github.com/aws/aws-sdk-go-v2/config"
	"github.com/aws/aws-sdk-go-v2/credentials"
	"github.com/aws/aws-sdk-go-v2/service/s3"
)

type Uploader struct {
	client *s3.Client
	bucket string
}

func NewUploader(endpointUrl, accessKey, secretKey, bucket string) (*Uploader, error) {
	cfg, err := config.LoadDefaultConfig(
		context.TODO(),
		config.WithCredentialsProvider(credentials.NewStaticCredentialsProvider(accessKey, secretKey, "")),
	)
	if err != nil {
		return nil, err
	}
	return &Uploader{
		client: s3.NewFromConfig(cfg, func(o *s3.Options) {
			o.BaseEndpoint = aws.String(endpointUrl)
		}),
		bucket: bucket,
	}, nil
}

func (u *Uploader) Exists(key string) (bool, error) {
	_, err := u.client.HeadObject(context.TODO(), &s3.HeadObjectInput{
		Bucket: aws.String(u.bucket),
		Key:    aws.String(key),
	})
	if err != nil {
		var responseError *http.ResponseError
		if errors.As(err, &responseError) && responseError.ResponseError.HTTPStatusCode() == 404 {
			return false, nil
		}
		return false, err
	}
	return true, nil
}

func (u *Uploader) Upload(srcPath, dstKey string) error {
	file, err := os.Open(srcPath)
	if err != nil {
		return err
	}
	defer func() { _ = file.Close() }()

	// TODO: use upload manager
	_, err = u.client.PutObject(context.TODO(), &s3.PutObjectInput{
		Bucket: aws.String(u.bucket),
		Key:    aws.String(dstKey),
		Body:   file,
	})
	return err
}