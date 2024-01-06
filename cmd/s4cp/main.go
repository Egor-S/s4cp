package main

import (
	"fmt"
	"os"

	"github.com/urfave/cli/v2"
)

func main() {
	app := &cli.App{
		Name:  "s4cp",
		Usage: "Copy SQLite database to S3",
		Flags: []cli.Flag{
			&cli.StringFlag{
				Name:     "endpoint-url",
				EnvVars:  []string{"ENDPOINT_URL"},
				Required: true,
			},
			&cli.StringFlag{
				Name:     "bucket",
				EnvVars:  []string{"BUCKET"},
				Required: true,
			},
			&cli.StringFlag{
				Name:     "access-key-id",
				EnvVars:  []string{"ACCESS_KEY_ID"},
				Required: true,
			},
			&cli.StringFlag{
				Name:     "secret-access-key",
				EnvVars:  []string{"SECRET_ACCESS_KEY"},
				Required: true,
			},
		},
		Action: func(context *cli.Context) error {
			// TODO: database arg
			// TODO: key arg
			return nil // TODO copy
		},
	}

	err := app.Run(os.Args)
	if err != nil {
		fmt.Println(err)
	}
}
