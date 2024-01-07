package s4cp

import (
	"compress/gzip"
	"fmt"
	"io"
	"log"
	"os"
	"strings"
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
	Gzip            bool

	Database string
	Key      string
}

func BackupToS3(options *Options) error {
	key := timefmt.Format(time.Now(), options.Key)
	if options.Gzip && !strings.HasSuffix(key, ".gz") {
		key += ".gz"
	}

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

	var file io.Reader
	if options.Gzip {
		gzipFile, err := os.CreateTemp("", "")
		if err != nil {
			return err
		}
		defer func() { _ = os.Remove(gzipFile.Name()) }()

		log.Println("Compressing backup")
		gzipWriter := gzip.NewWriter(gzipFile)
		_, err = io.Copy(gzipWriter, tempFile)
		if err != nil {
			return err
		}

		err = gzipWriter.Close()
		if err != nil {
			return err
		}

		_, err = gzipFile.Seek(0, io.SeekStart)
		if err != nil {
			return err
		}

		file = gzipFile
	}

	log.Printf("Uploading %s to S3\n", key)
	err = uploader.Upload(file, key)
	if err != nil {
		return err
	}

	log.Println("Backup completed successfully")
	return nil
}
