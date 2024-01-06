package s4cp

import (
	"fmt"
	"log"
	"os"

	"github.com/Egor-S/s4cp/internal/s3"
	"github.com/Egor-S/s4cp/internal/sqlite"
)

type Options struct {
	Bucket          string
	EndpointUrl     string
	AccessKeyId     string
	SecretAccessKey string

	Database string
	Key      string
}

func BackupToS3(options *Options) error {
	uploader, err := s3.NewUploader(options.EndpointUrl, options.AccessKeyId, options.SecretAccessKey, options.Bucket)
	if err != nil {
		return err
	}

	exists, err := uploader.Exists(options.Key)
	if err != nil {
		return err
	}
	if exists {
		return fmt.Errorf("key %s already exists", options.Key)
	}

	log.Println("Backing up database to temporary file")
	tempFile, err := os.CreateTemp("", "")
	if err != nil {
		return err
	}
	defer func() { _ = os.Remove(tempFile.Name()) }()

	err = sqlite.Backup(options.Database, tempFile.Name())
	if err != nil {
		return err
	}

	log.Println("Uploading to S3")
	err = uploader.Upload(tempFile.Name(), options.Key)
	if err != nil {
		return err
	}

	log.Println("Backup completed successfully")
	return nil
}
