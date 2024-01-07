package s4cp

import (
	"fmt"
	"log"
	"os"
	"time"

	"github.com/Egor-S/s4cp/internal/s3"
	"github.com/Egor-S/s4cp/internal/sqlite"
	"github.com/itchyny/timefmt-go"
)

type Options struct {
	Bucket          string
	EndpointUrl     string
	Region          string
	AccessKeyId     string
	SecretAccessKey string

	Database string
	Key      string
}

func BackupToS3(options *Options) error {
	key := timefmt.Format(time.Now(), options.Key)
	uploader, err := s3.NewUploader(options.EndpointUrl, options.Region, options.AccessKeyId, options.SecretAccessKey, options.Bucket)
	if err != nil {
		return err
	}

	exists, err := uploader.Exists(key)
	if err != nil {
		return err
	}
	if exists {
		return fmt.Errorf("key %s already exists", key)
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

	log.Printf("Uploading %s to S3\n", key)
	err = uploader.Upload(tempFile.Name(), key)
	if err != nil {
		return err
	}

	log.Println("Backup completed successfully")
	return nil
}
