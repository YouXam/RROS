// SPDX-License-Identifier: GPL-2.0
/*
 * rpi-poe-power.c - Raspberry Pi PoE+ HAT power supply driver.
 *
 * Copyright (C) 2019 Raspberry Pi (Trading) Ltd.
 * Based on axp20x_ac_power.c by Quentin Schulz <quentin.schulz@free-electrons.com>
 *
 * Author: Serge Schneider <serge@raspberrypi.org>
 */

#include <linux/module.h>
#include <linux/of.h>
#include <linux/platform_device.h>
#include <linux/power_supply.h>
#include <soc/bcm2835/raspberrypi-firmware.h>

#define RPI_POE_ADC_REG			0x2
#define RPI_POE_FLAG_REG		0x4

#define RPI_POE_FLAG_AT			BIT(0)
#define RPI_POE_FLAG_OC			BIT(1)

#define RPI_POE_CURRENT_AF_MAX	(2500 * 1000)
#define RPI_POE_CURRENT_AT_MAX	(5000 * 1000)

#define DRVNAME "rpi-poe-power-supply"

struct rpi_poe_power_supply_ctx {
	struct power_supply *supply;
	struct rpi_firmware *fw;
};

struct fw_tag_data_s {
	u32 reg;
	u32 val;
	u32 ret;
};

static int write_reg(struct rpi_firmware *fw, u32 reg, u32 *val)
{
	struct fw_tag_data_s fw_tag_data = {
		.reg = reg,
		.val = *val
	};
	int ret;

	ret = rpi_firmware_property(fw, RPI_FIRMWARE_SET_POE_HAT_VAL,
				    &fw_tag_data, sizeof(fw_tag_data));
	if (ret)
		return ret;
	else if (fw_tag_data.ret)
		return -EIO;
	return 0;
}

static int read_reg(struct rpi_firmware *fw, u32 reg, u32 *val)
{
	struct fw_tag_data_s fw_tag_data = {
		.reg = reg,
		.val = *val
	};
	int ret;

	ret = rpi_firmware_property(fw, RPI_FIRMWARE_GET_POE_HAT_VAL,
				    &fw_tag_data, sizeof(fw_tag_data));
	if (ret)
		return ret;
	else if (fw_tag_data.ret)
		return -EIO;

	*val = fw_tag_data.val;
	return 0;
}

static int rpi_poe_power_supply_get_property(struct power_supply *psy,
					enum power_supply_property psp,
					union power_supply_propval *r_val)
{
	struct rpi_poe_power_supply_ctx *ctx = power_supply_get_drvdata(psy);
	int ret;
	unsigned int val = 0;

	switch (psp) {
	case POWER_SUPPLY_PROP_HEALTH:
		ret = read_reg(ctx->fw, RPI_POE_FLAG_REG, &val);
		if (ret)
			return ret;

		if (val & RPI_POE_FLAG_OC) {
			r_val->intval = POWER_SUPPLY_HEALTH_UNSPEC_FAILURE;
			val = RPI_POE_FLAG_OC;
			ret = write_reg(ctx->fw, RPI_POE_FLAG_REG, &val);
			if (ret)
				return ret;
			return 0;
		}

		r_val->intval = POWER_SUPPLY_HEALTH_GOOD;
		return 0;

	case POWER_SUPPLY_PROP_ONLINE:
		ret = read_reg(ctx->fw, RPI_POE_ADC_REG, &val);
		if (ret)
			return ret;

		r_val->intval = (val > 5);
		return 0;

	case POWER_SUPPLY_PROP_CURRENT_AVG:
		val = 50;
		ret = read_reg(ctx->fw, RPI_POE_ADC_REG, &val);
		if (ret)
			return ret;
		val = (val * 3300)/9821;
		r_val->intval = val * 1000;
		return 0;

	case POWER_SUPPLY_PROP_CURRENT_NOW:
		ret = read_reg(ctx->fw, RPI_POE_ADC_REG, &val);
		if (ret)
			return ret;
		val = (val * 3300)/9821;
		r_val->intval = val * 1000;
		return 0;

	case POWER_SUPPLY_PROP_CURRENT_MAX:
		ret = read_reg(ctx->fw, RPI_POE_FLAG_REG, &val);
		if (ret)
			return ret;

		if (val & RPI_POE_FLAG_AT) {
			r_val->intval = RPI_POE_CURRENT_AT_MAX;
			return 0;
		}
		r_val->intval = RPI_POE_CURRENT_AF_MAX;
		return 0;

	default:
		return -EINVAL;
	}

	return -EINVAL;
}

static enum power_supply_property rpi_poe_power_supply_properties[] = {
	POWER_SUPPLY_PROP_HEALTH,
	POWER_SUPPLY_PROP_ONLINE,
	POWER_SUPPLY_PROP_CURRENT_AVG,
	POWER_SUPPLY_PROP_CURRENT_NOW,
	POWER_SUPPLY_PROP_CURRENT_MAX,
};

static const struct power_supply_desc rpi_poe_power_supply_desc = {
	.name = "rpi-poe",
	.type = POWER_SUPPLY_TYPE_MAINS,
	.properties = rpi_poe_power_supply_properties,
	.num_properties = ARRAY_SIZE(rpi_poe_power_supply_properties),
	.get_property = rpi_poe_power_supply_get_property,
};

static int rpi_poe_power_supply_probe(struct platform_device *pdev)
{
	struct power_supply_config psy_cfg = {};
	struct rpi_poe_power_supply_ctx *ctx;
	struct device_node *fw_node;
	u32 revision;

	if (!of_device_is_available(pdev->dev.of_node))
		return -ENODEV;

	fw_node = of_parse_phandle(pdev->dev.of_node, "firmware", 0);
	if (!fw_node) {
		dev_err(&pdev->dev, "Missing firmware node\n");
		return -ENOENT;
	}

	ctx = devm_kzalloc(&pdev->dev, sizeof(*ctx), GFP_KERNEL);
	if (!ctx)
		return -ENOMEM;

	ctx->fw = rpi_firmware_get(fw_node);
	if (!ctx->fw)
		return -EPROBE_DEFER;
	if (rpi_firmware_property(ctx->fw,
			RPI_FIRMWARE_GET_FIRMWARE_REVISION,
			&revision, sizeof(revision))) {
		dev_err(&pdev->dev, "Failed to get firmware revision\n");
		return -ENOENT;
	}
	if (revision < 0x60af72e8) {
		dev_err(&pdev->dev, "Unsupported firmware\n");
		return -ENOENT;
	}
	platform_set_drvdata(pdev, ctx);

	psy_cfg.of_node = pdev->dev.of_node;
	psy_cfg.drv_data = ctx;

	ctx->supply = devm_power_supply_register(&pdev->dev,
						   &rpi_poe_power_supply_desc,
						   &psy_cfg);
	if (IS_ERR(ctx->supply))
		return PTR_ERR(ctx->supply);

	return 0;
}

static const struct of_device_id of_rpi_poe_power_supply_match[] = {
	{ .compatible = "raspberrypi,rpi-poe-power-supply", },
	{ /* sentinel */ }
};
MODULE_DEVICE_TABLE(of, of_rpi_poe_power_supply_match);

static struct platform_driver rpi_poe_power_supply_driver = {
	.probe = rpi_poe_power_supply_probe,
	.driver = {
		.name = DRVNAME,
		.of_match_table = of_rpi_poe_power_supply_match
	},
};

module_platform_driver(rpi_poe_power_supply_driver);

MODULE_AUTHOR("Serge Schneider <serge@raspberrypi.org>");
MODULE_ALIAS("platform:" DRVNAME);
MODULE_DESCRIPTION("Raspberry Pi PoE+ HAT power supply driver");
MODULE_LICENSE("GPL");
