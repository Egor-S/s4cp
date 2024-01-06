package main

import (
	"fmt"
	"os"

	s4cp "github.com/Egor-S/s4cp/internal"
	"github.com/urfave/cli/v2"
)

func main() {
	options := &s4cp.Options{}
	app := &cli.App{
		Name:  "s4cp",
		Usage: "Copy SQLite database to S3",
		Flags: []cli.Flag{
			&cli.StringFlag{
				Name:        "bucket",
				EnvVars:     []string{"BUCKET"},
				Destination: &options.Bucket,
				Required:    true,
			},
			&cli.StringFlag{
				Name:        "endpoint-url",
				EnvVars:     []string{"ENDPOINT_URL"},
				Destination: &options.EndpointUrl,
				Required:    true,
			},
			&cli.StringFlag{
				Name:        "access-key-id",
				EnvVars:     []string{"ACCESS_KEY_ID"},
				Destination: &options.AccessKeyId,
				Required:    true,
			},
			&cli.StringFlag{
				Name:        "secret-access-key",
				EnvVars:     []string{"SECRET_ACCESS_KEY"},
				Destination: &options.SecretAccessKey,
				Required:    true,
			},
		},
		Action: func(context *cli.Context) error {
			options.Database = context.Args().Get(0)
			if options.Database == "" {
				return cli.Exit("missing source database argument", 1)
			}
			options.Key = context.Args().Get(1)
			if options.Key == "" {
				return cli.Exit("missing destination key argument", 1)
			}

			return s4cp.BackupToS3(options)
		},
	}

	err := app.Run(os.Args)
	if err != nil {
		fmt.Println(err)
	}
}
